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

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;

use forge_shim::{
    Repository, install_failing_gh, install_repository_gh_failing, path_with_gh_shim, pull_record,
    pull_record_with_fields,
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
/// pull requests #7 and #12 (`track`'s tests add #16 and #17 by
/// [`Fixture::forge`]); the fork's own repository has, from `acme`'s
/// branches, #12 (head `feat/alpha`), #13 (head `feat/base`), #14 (head
/// `feat/landed`), #15 (merged, head `feat/base`), #16 and #17 (head
/// `feat/alpha`), #21 (head `main`, at its commit) and #22 (merged into
/// `review/base`, head `feat/gone`, a branch no longer here), and #20 from
/// `bob`'s `main` (at its commit); and a sibling fork, whose own repository
/// `acme/sibling` has #5 (head `sib/feature`) and #6 (head `feat/landed`, the
/// sibling's own branch of that name).
struct Fixture {
    lab: Lab,
    home: tempfile::TempDir,
    shim: tempfile::TempDir,
    log: PathBuf,
    forge: RefCell<ForgeState>,
    /// What this fork's ledger is kept under, as the code names it: its
    /// upstream's lowercase `<owner>/<name>`, or its registry key, `demo`,
    /// when its upstream is a path.
    kept_under: String,
}

/// What the fixture's fake forge answers beyond its fixed pull requests:
/// the head commit of each of the fork's #12 to #15, #20 and #21, pull
/// requests added to the upstream, repositories it hides (answering as
/// GitHub does for a repository the token cannot see, or that does not
/// exist: the two read the same), and repositories whose every call fails
/// as an outage does.
#[derive(Default)]
struct ForgeState {
    heads: BTreeMap<u64, String>,
    upstream: Vec<String>,
    hidden: Vec<&'static str>,
    failing: Vec<&'static str>,
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
    fixture_on(UPSTREAM, origin, registered)
}

/// [`fixture_with`] with the fork's upstream at `upstream`. An empty
/// `upstream` leaves it the lab's path, so this fork's upstream names no
/// forge repository and its ledger is kept under its registry key, `demo`;
/// an empty `registered` makes the registry's `origin` the lab's origin path.
fn fixture_on(upstream: &str, origin: &str, registered: &str) -> Fixture {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/base", "base.txt", "base\n");
    lab.branch("feat/landed", "landed.txt", "landed\n");
    lab.upstream_trunk_file("landed.txt", "landed\n");
    let upstream = if upstream.is_empty() {
        lab.upstream.display().to_string()
    } else {
        lab.upstream_at_forge_url(upstream);
        upstream.to_owned()
    };
    // An empty `origin` leaves the checkout's `origin` remote a local path,
    // which names no forge repository.
    if !origin.is_empty() {
        lab.origin_at_forge_url(origin);
    }
    let registered = if registered.is_empty() {
        lab.temp_origin().display().to_string()
    } else {
        registered.to_owned()
    };
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{upstream}\"\norigin = \"{registered}\"\n\
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
            (20, tip("main@origin")),
            (21, tip("main@origin")),
        ]),
        ..ForgeState::default()
    };
    install_forge(shim.path(), &log, &state);
    Fixture {
        kept_under: knives::remote_url::remote_slug(&upstream)
            .map_or_else(|| "demo".to_owned(), str::to_ascii_lowercase),
        lab,
        home,
        shim,
        log,
        forge: RefCell::new(state),
    }
}

/// Every file under `root`, at any depth, but none under a directory or file
/// whose name starts with `.`; none when `root` does not exist.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
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
    let at = |number: u64, pull_state: &str, head: &str, owner: &str| {
        pull_record_with_fields(
            number,
            pull_state,
            head,
            &format!(
                ",\"headRefOid\":\"{}\",\"headRepositoryOwner\":{{\"login\":\"{owner}\"}}",
                state.heads[&number]
            ),
        )
    };
    let fork_pulls = format!(
        "[{},{},{},{},{},{},{},{},{}]",
        at(12, "OPEN", "feat/alpha", "acme"),
        at(13, "OPEN", "feat/base", "acme"),
        at(14, "OPEN", "feat/landed", "acme"),
        at(15, "MERGED", "feat/base", "acme"),
        review(16, "OPEN", "feat/alpha", "acme"),
        review(17, "OPEN", "feat/alpha", "acme"),
        at(20, "OPEN", "main", "bob"),
        at(21, "OPEN", "main", "acme"),
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
    let other_pulls = format!("[{}]", review(12, "OPEN", "feat/base", "acme"));
    let sibling_upstream_pulls = format!(
        "[{},{}]",
        pull_record(51, "MERGED", "sib/merged", None),
        pull_record(52, "OPEN", "sib/open", None)
    );
    let repositories = [
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
        // A former name nothing here spells, which the forge resolves to
        // the fork's own repository.
        Repository {
            named: "acme-older/demo",
            canonical: "acme/demo",
            pulls: "[]",
        },
        Repository {
            named: "acme/other",
            canonical: "acme/other",
            pulls: &other_pulls,
        },
        Repository {
            named: "maintainer/sibling",
            canonical: "maintainer/sibling",
            pulls: &sibling_upstream_pulls,
        },
        Repository {
            named: "acme/sibling",
            canonical: "acme/sibling",
            pulls: &sibling_pulls,
        },
    ];
    let visible: Vec<Repository<'_>> = repositories
        .into_iter()
        .filter(|repository| !state.hidden.contains(&repository.named))
        .collect();
    install_repository_gh_failing(shim, &visible, &state.failing, log);
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

    /// Every entry of this fork's ledger. Every file under the ledger root
    /// must be this fork's: a ledger written under another name would leave
    /// this one empty, and a "nothing recorded" check would pass without
    /// reading what was written. So the ledger read is either found, or the
    /// root holds no file at all.
    fn entries(&self) -> Vec<Entry> {
        let root = self.home.path().join("ledger");
        let dir = root.join(&self.kept_under);
        let stray: Vec<PathBuf> = files_under(&root)
            .into_iter()
            .filter(|file| !file.starts_with(&dir))
            .collect();
        assert!(
            stray.is_empty(),
            "the ledger holds files outside {}: {stray:?}",
            dir.display()
        );
        let read = Ledger::at(dir).read().expect("read the ledger");
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

    /// Reinstall the fixture's forge with `change` made to what it answers.
    fn forge_as(&self, change: impl FnOnce(&mut ForgeState)) {
        let mut state = self.forge.borrow_mut();
        change(&mut state);
        install_forge(self.shim.path(), &self.log, &state);
    }

    fn forge_calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Rewrite the one ledger entry of the fork that states the list `from`
    /// to state `to`: a record an older knives, or another machine, could
    /// have written, which this knives refuses to write.
    fn rewrite_requirements(&self, from: &str, to: &str) {
        let rewritten =
            self.rewrite_ledger(&format!("value = \"{from}\""), &format!("value = \"{to}\""));
        assert_eq!(rewritten, 1, "no one entry states {from}");
    }

    /// Replace `from` with `to` in every ledger entry of the fork that holds
    /// it, at least one; how many.
    fn rewrite_ledger(&self, from: &str, to: &str) -> usize {
        self.rewrite_entries(None, from, to)
    }

    /// [`Self::rewrite_ledger`] in the entries about `subject` alone, when
    /// it is given.
    fn rewrite_entries(&self, subject: Option<&str>, from: &str, to: &str) -> usize {
        let dir = self.home.path().join("ledger").join(&self.kept_under);
        let about = subject.map(|subject| format!("subject = \"{subject}\""));
        let mut rewritten = 0;
        for file in std::fs::read_dir(&dir).expect("read the ledger directory") {
            let path = file.expect("a ledger directory entry").path();
            if path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a ledger entry");
            if text.contains(from) && about.as_ref().is_none_or(|about| text.contains(about)) {
                std::fs::write(&path, text.replace(from, to)).expect("rewrite a ledger entry");
                rewritten += 1;
            }
        }
        assert!(rewritten > 0, "no entry holds {from} in {}", dir.display());
        rewritten
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
    let recorded = fixture.depends(&["--on", "origin:acme/demo#13", "--on", "acme/demo#14"]);
    assert_eq!(
        recorded.trim_end(),
        "demo/feat/alpha now requires origin:acme/demo#13, origin:acme/demo#14"
    );

    // And: each is forgotten by the registry name or an old spelling, with
    // and without its `origin:` prefix.
    fixture.depends_of("feat/base", &["--forget", "demo#12"]);
    fixture.depends(&["--forget", "acme-old/demo#13"]);
    fixture.depends(&["--forget", "origin:acme-old/demo#14"]);
    assert_eq!(fixture.required("feat/base"), Vec::<String>::new());
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_record_from_before_a_rename_is_forgotten_by_the_new_spellings() {
    // Given: a fork whose checkout and registry both spell its own repository
    // by its new name, `acme/demo`, and feat/alpha's requirements recorded
    // under the old one, `acme-old/demo`, as a machine wrote them before the
    // rename.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    fixture.depends(&[
        "--on",
        "acme/demo#13",
        "--on",
        "acme/demo#14",
        "--on",
        "acme/demo#15",
    ]);
    fixture.rewrite_requirements(
        "origin:acme/demo#13,origin:acme/demo#14,origin:acme/demo#15",
        "origin:acme-old/demo#13,origin:acme-old/demo#14,origin:acme-old/demo#15",
    );

    // When / Then: the registry name, the new name and the new name's token
    // each take one back.
    for (forget, left) in [
        (
            "demo#13",
            &["origin:acme-old/demo#14", "origin:acme-old/demo#15"][..],
        ),
        ("acme/demo#14", &["origin:acme-old/demo#15"][..]),
        ("origin:acme/demo#15", &[][..]),
    ] {
        fixture.depends(&["--forget", forget]);
        assert_eq!(fixture.required("feat/alpha"), left, "{forget}");
    }
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
fn track_refuses_a_number_whose_fork_pull_request_is_the_branchs_own() {
    // Given: #12 is on both: the upstream's, head upstream-twelve, belongs to
    // someone else, and the fork's has feat/alpha as its head.
    let fixture = fixture();

    // When: feat/alpha is said to be #12.
    let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", "12"]);

    // Then: refused, naming both pull requests: #12 is feat/alpha's review
    // pull request on the fork, not the upstream one it becomes.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for named in ["maintainer/demo#12", "upstream-twelve", "acme/demo#12"] {
        assert!(stderr.contains(named), "{named}: {stderr}");
    }
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn track_reads_the_checkouts_origin_as_status_and_depends_do() {
    // Given: the checkout's `origin` remote names the fork's own repository,
    // acme/demo, and the registry names another live one, acme/other, with
    // no pull requests. The fork's #12 has feat/alpha as its head; the
    // upstream's #12 is someone else's.
    let fixture = fixture_with(ORIGIN, "https://forge.invalid/acme/other");

    // When: feat/alpha is said to be #12.
    let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", "12"]);

    // Then: #12 is judged on the repository the checkout names, as `status`
    // and `depends` judge it: feat/alpha's own review pull request, refused.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("acme/demo#12"), "{stderr}");
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn track_falls_back_to_the_registry_when_the_checkouts_origin_is_a_local_path() {
    // Given: the checkout's `origin` remote is a local path, which names no
    // forge repository; the registry names the fork's own repository,
    // acme/demo, whose #12 has feat/alpha as its head.
    let fixture = fixture_on(UPSTREAM, "", ORIGIN);

    // When: feat/alpha is said to be #12.
    let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", "12"]);

    // Then: the check still runs, on the registry's repository: refused.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("acme/demo#12"), "{stderr}");
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn track_judges_a_head_by_the_forges_owner_not_a_former_one() {
    // Given: the registry still spells the fork's own repository by its
    // former owner, acme-old; the forge names it acme/demo. #17 is on both:
    // the upstream's has acme-old's branch feat/alpha as its head, the fork's
    // has acme's feat/alpha.
    let fixture = fixture();
    fixture.forge(&[review(17, "MERGED", "feat/alpha", "acme-old")]);

    // When: feat/alpha is said to be #17.
    let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", "17"]);

    // Then: the former owner's same-named branch is not feat/alpha's: #17
    // is its review pull request on the fork, and is refused, naming both.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for named in ["maintainer/demo#17", "acme-old:feat/alpha", "acme/demo#17"] {
        assert!(stderr.contains(named), "{named}: {stderr}");
    }
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn track_refuses_a_number_whose_upstream_head_only_shares_the_branchs_name() {
    // Given: #16 is on both: the upstream's has bob's branch feat/alpha as its
    // head, and the fork's has acme's feat/alpha.
    let fixture = fixture();
    fixture.forge(&[review(16, "OPEN", "feat/alpha", "bob")]);

    // When: feat/alpha is said to be #16.
    let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", "16"]);

    // Then: the shared name does not make the upstream's #16 feat/alpha's:
    // refused, naming both, as for any other head.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for named in ["maintainer/demo#16", "acme/demo#16"] {
        assert!(stderr.contains(named), "{named}: {stderr}");
    }
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn track_states_a_number_the_upstream_has() {
    // Given: #7 is on the upstream only; #12 is on both, and the fork's #12
    // is feat/alpha's, not feat/base's.
    let fixture = fixture();

    // When: #7 is stated for feat/alpha, and #12 for feat/base, a branch
    // carrying someone else's upstream pull request.
    let mut said_lines = Vec::new();
    for (branch, number) in [("feat/alpha", "7"), ("feat/base", "12")] {
        let output = fixture.knives(&["--text", "track", branch, "--pr", number]);
        assert!(output.status.success(), "#{number}: {}", said(&output));
        said_lines.push(
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned(),
        );
    }

    // Then: both are recorded, as `track` reads a number: the upstream's.
    assert_eq!(
        said_lines,
        ["demo/feat/alpha is #7", "demo/feat/base is #12"]
    );
    let live = Statements::from_entries(&fixture.entries());
    assert_eq!(live.pull("feat/alpha"), Some(7));
    assert_eq!(live.pull("feat/base"), Some(12));
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
    // Given: feat/alpha and feat/base both require the fork's #14, whose head
    // feat/landed has landed: met.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#14"]);
    fixture.depends_of("feat/base", &["--on", "acme/demo#14"]);
    let (code, report) = status(&fixture);
    assert_eq!(details(&report, "unmet-dependency"), Vec::<String>::new());
    assert_eq!(problems(&report), Vec::<String>::new(), "{report}");
    assert_ne!(code, Some(3), "{report}");

    // When: the forge reports #14's head at a commit this checkout holds but
    // the local feat/landed is not at: feat/base's tip, whose work has not
    // landed, as after a force-push to a commit already here.
    let base = lab::commit_at(&fixture.lab, "feat/base");
    fixture.move_head(14, base.as_str());
    let (code, report) = status(&fixture);

    // Then: that commit is judged, not the landed branch of the same name,
    // for each dependent alike: the second reads the first's judgement.
    let unmet = |dependent: &str| {
        format!(
            "{dependent} requires origin:acme/demo#14 (head feat/landed, open), which has not \
             landed (feat/landed at {} not-in-trunk)",
            base.short()
        )
    };
    assert_eq!(
        details(&report, "unmet-dependency"),
        [unmet("feat/alpha"), unmet("feat/base")],
        "{report}"
    );
    assert_eq!(problems(&report), Vec::<String>::new(), "{report}");
    assert_eq!(code, Some(1), "{report}");

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
    assert!(problem.contains("refs/pull/22/head"), "{problem}");
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

#[test]
fn forgetting_a_member_token_of_another_repository_is_refused() {
    // Given: feat/alpha requires the member branch feat/base.
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);

    // When / Then: feat/base named on the sibling's repository is refused as
    // `--on` refuses it. Named on a repository the forge does not name, which
    // could be a former name of the fork's own that the token cannot see,
    // nothing is taken back (exit 3), and the refusal names the branch name
    // that takes it back.
    fixture.refused(
        "feat/alpha",
        &["--forget", "origin:acme/sibling:feat/base"],
        &["which is sibling's own repository, not demo's"],
    );
    let before = fixture.entries().len();
    let output = fixture.knives(&[
        "--text",
        "depends",
        "feat/alpha",
        "--forget",
        "origin:someone/else:feat/base",
    ]);
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("`knives depends feat/alpha --forget feat/base`"),
        "{}",
        said(&output)
    );
    assert_eq!(
        fixture.entries().len(),
        before,
        "the refusal wrote an entry"
    );
    assert_eq!(
        fixture.required("feat/alpha"),
        ["origin:acme/demo:feat/base"]
    );
}

#[test]
fn a_member_whose_origin_holds_work_its_local_tip_lacks_stays_unmet() {
    // Given: feat/base, pushed and squash-merged upstream as #40; feat/alpha
    // requires it: met, by #40's landing.
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
    fixture.depends(&["--on", "feat/base"]);
    let (_, control) = status(&fixture);
    assert_eq!(
        details(&control, "unmet-dependency"),
        Vec::<String>::new(),
        "{control}"
    );

    // When: this checkout stops tracking feat/base, another clone advances
    // it on origin with new work, and the fetch moves only feat/base@origin:
    // the local bookmark stays at the merged tip, which its row judges.
    lab.jj_work(["bookmark", "untrack", "feat/base", "--remote", "origin"]);
    lab.advance_origin_branch("feat/base", "new work on origin\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    assert_eq!(lab::commit_at(lab, "feat/base"), merged);
    assert_ne!(lab::commit_at(lab, "feat/base@origin"), merged);
    let (code, report) = status(&fixture);

    // Then: the origin tip is judged too, as a machine holding only it
    // judges it, and its new work has not landed.
    let unmet = details(&report, "unmet-dependency");
    assert!(
        unmet.iter().any(|detail| detail.starts_with(
            "feat/alpha requires origin:acme/demo:feat/base, which has not landed \
             (feat/base@origin "
        )),
        "{unmet:?}\n{report}"
    );
    assert_ne!(code, Some(0), "{report}");
}

#[test]
fn a_deleted_dependent_is_checked_by_its_recorded_head_or_reported_unknown() {
    // Given: feat/dep and feat/done each require feat/base, which has not
    // landed; the trunk has feat/done's work and not feat/dep's. Both are
    // then deleted here, never pushed, as after merging into a review base.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/dep", "dep.txt", "dep\n");
    lab.branch("feat/done", "done.txt", "done\n");
    lab.upstream_trunk_file("done.txt", "done\n");
    fixture.depends_of("feat/dep", &["--on", "feat/base"]);
    fixture.depends_of("feat/done", &["--on", "feat/base"]);
    let done = lab::commit_at(lab, "feat/done");
    lab.jj_work(["bookmark", "delete", "feat/dep", "feat/done"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: feat/dep's recorded commit has not landed, so its requirement is
    // checked, and unmet. feat/done's recorded commit landed, which does not
    // show that later work it carried did: not silent, a problem naming what
    // could not be read.
    let unmet = details(&report, "unmet-dependency");
    assert!(
        unmet.contains(
            &"feat/dep requires origin:acme/demo:feat/base, which has not landed (no-pr)"
                .to_owned()
        ),
        "{unmet:?}\n{report}"
    );
    assert!(
        !unmet
            .iter()
            .any(|detail| detail.starts_with("feat/done requires")),
        "{unmet:?}\n{report}"
    );
    assert!(
        problems(&report).contains(&format!(
            "cannot tell whether feat/done still needs origin:acme/demo:feat/base: its branch is \
             gone here and on origin, and the newest commit the ledger records for it, {}, has \
             landed, which does not show that work it carried later did; \
             origin:acme/demo:feat/base, which has not landed (no-pr). Forget these once its \
             work has landed or been dropped: `knives depends feat/done --forget feat/base`",
            done.short()
        )),
        "{report}"
    );
    assert_eq!(code, Some(3), "{report}");
}

#[test]
fn a_deleted_dependent_whose_requirements_are_all_met_says_nothing() {
    // Given: feat/done requires only feat/landed, which has landed; the
    // trunk has feat/done's own work too; then feat/done is deleted.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/done", "done.txt", "done\n");
    lab.upstream_trunk_file("done.txt", "done\n");
    fixture.depends_of("feat/done", &["--on", "feat/landed"]);
    lab.jj_work(["bookmark", "delete", "feat/done"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: what feat/done carried later cannot change a met requirement, so
    // nothing is said of it.
    assert!(
        !problems(&report)
            .iter()
            .any(|problem| problem.contains("feat/done")),
        "{report}"
    );
    assert_ne!(code, Some(3), "{report}");
}

#[test]
fn a_deleted_dependents_absent_commit_is_fetched_to_judge_it() {
    // Given: feat/gone requires feat/base, which has not landed; the ledger's
    // record of feat/gone's commit names one this checkout does not hold.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/gone", "gone.txt", "gone\n");
    fixture.depends_of("feat/gone", &["--on", "feat/base"]);
    let held = lab::commit_at(lab, "feat/gone");
    fixture.rewrite_ledger(held.as_str(), &"d".repeat(40));
    lab.jj_work(["bookmark", "delete", "feat/gone"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: the problem says a fetch can judge it, and gives the command.
    let problems = problems(&report);
    assert!(
        problems.iter().any(|problem| problem.starts_with(&format!(
            "cannot tell whether feat/gone still needs origin:acme/demo:feat/base: its branch is \
             gone here and on origin, and the newest commit the ledger records for it, {}, is \
             not in this checkout; origin:acme/demo:feat/base, which has not landed (no-pr). \
             Fetch a ref that holds its work to judge it, or forget these once its work has \
             landed or been dropped: `knives depends feat/gone --forget feat/base`",
            "d".repeat(12)
        ))),
        "{problems:?}\n{report}"
    );
    assert_eq!(code, Some(3), "{report}");
}

#[test]
fn status_on_github_remotes_keeps_another_owners_pull_request_off_a_member() {
    // Given: an upstream pull request from bob's branch named feat/alpha, and
    // feat/alpha requiring the fork's #13. On github.com remotes, status
    // takes as the fork's only the upstream pull requests of its owners; on
    // the test host no owner is read from a remote, so every one is taken.
    for (upstream, origin, taken) in [
        (
            "https://github.com/maintainer/demo",
            "https://github.com/acme/demo",
            false,
        ),
        (UPSTREAM, ORIGIN, true),
    ] {
        let fixture = fixture_on(upstream, origin, origin);
        fixture.forge(&[pull_record_with_fields(
            16,
            "OPEN",
            "feat/alpha",
            ",\"headRepositoryOwner\":{\"login\":\"bob\"},\"mergeable\":\"MERGEABLE\",\
             \"mergeStateStatus\":\"CLEAN\"",
        )]);
        fixture.depends(&["--on", "acme/demo#13"]);

        // When: status reports the fork.
        let (code, report) = status(&fixture);

        // Then: bob's pull request is feat/alpha's only on the test host, and
        // the requirement is checked on both.
        let alpha = report["branches"]
            .as_array()
            .expect("branches")
            .iter()
            .find(|row| row["name"] == "feat/alpha")
            .expect("a feat/alpha row");
        assert_eq!(alpha["state"] != "no-pr", taken, "{upstream}: {report}");
        assert!(
            details(&report, "unmet-dependency")
                .iter()
                .any(|detail| detail.starts_with(
                    "feat/alpha requires origin:acme/demo#13 (head feat/base, open), which has not \
                 landed ("
                )),
            "{upstream}: {report}"
        );
        assert_eq!(code, Some(1), "{upstream}: {report}");
    }
}

/// The configs a renamed fork's repository can be in: its checkout and
/// registry on the old name, `acme-old/demo`; both on the new one; or the
/// checkout on the new name and the registry on the old.
const RENAMED_CONFIGS: [(&str, &str); 3] = [
    (REGISTRY_ORIGIN, REGISTRY_ORIGIN),
    (ORIGIN, ORIGIN),
    (ORIGIN, REGISTRY_ORIGIN),
];

#[test]
fn a_review_pull_request_held_under_both_names_is_forgotten_by_any_spelling() {
    for (origin, registered) in RENAMED_CONFIGS {
        let fixture = fixture_with(origin, registered);
        for forget in [
            "origin:acme/demo#13",
            "origin:acme-old/demo#13",
            "acme/demo#13",
            "acme-old/demo#13",
            "demo#13",
        ] {
            // Given: feat/alpha requires the fork's #13 under both the old
            // name and the new, as when a record from before the rename was
            // written again after it.
            fixture.depends(&["--on", "acme/demo#13"]);
            fixture.rewrite_requirements(
                "origin:acme/demo#13",
                "origin:acme-old/demo#13,origin:acme/demo#13",
            );
            assert_eq!(
                fixture.required("feat/alpha"),
                ["origin:acme-old/demo#13", "origin:acme/demo#13"],
                "{origin}: {forget}"
            );

            // When / Then: any spelling takes both back: one requirement.
            fixture.depends(&["--forget", forget]);
            assert_eq!(
                fixture.required("feat/alpha"),
                Vec::<String>::new(),
                "{origin}: {forget}"
            );
        }
    }
}

#[test]
fn naming_a_review_pull_request_again_keeps_the_spelling_held() {
    for (origin, registered) in RENAMED_CONFIGS {
        // Given: feat/alpha requires the fork's #13 under its old name only.
        let fixture = fixture_with(origin, registered);
        fixture.depends(&["--on", "acme/demo#13"]);
        fixture.rewrite_requirements("origin:acme/demo#13", "origin:acme-old/demo#13");

        // When: it is named again by the new name.
        let named = fixture.depends(&["--on", "acme/demo#13"]);

        // Then: the record held is named, and no second spelling is written.
        assert_eq!(
            named.trim_end(),
            "demo/feat/alpha now requires origin:acme-old/demo#13",
            "{origin}"
        );
        assert_eq!(
            fixture.required("feat/alpha"),
            ["origin:acme-old/demo#13"],
            "{origin}"
        );
    }
}

#[test]
fn an_exact_upstream_forget_needs_no_forge_answer() {
    // Given: feat/base requires the upstream's #12 and the fork's #12.
    let fixture = fixture();
    fixture.depends_of(
        "feat/base",
        &["--on", "maintainer/demo#12", "--on", "acme/demo#12"],
    );

    // When: the upstream's is taken back, named as its record spells it,
    // with a forge that answers.
    std::fs::write(&fixture.log, "").expect("clear the forge call log");
    fixture.depends_of("feat/base", &["--forget", "maintainer/demo#12"]);

    // Then: the forge was not asked, and only the upstream's is gone.
    assert_eq!(fixture.forge_calls(), "");
    assert_eq!(fixture.required("feat/base"), ["origin:acme/demo#12"]);

    // And: with the forge down, the same forget still works.
    fixture.depends_of("feat/base", &["--on", "maintainer/demo#12"]);
    install_failing_gh(fixture.shim.path(), &fixture.log);
    fixture.depends_of("feat/base", &["--forget", "maintainer/demo#12"]);
    assert_eq!(fixture.required("feat/base"), ["origin:acme/demo#12"]);
}

#[test]
fn an_ambiguous_forget_while_the_forge_is_down_takes_nothing_back() {
    // Given: the checkout and registry on the old name; feat/base requires
    // the upstream's #12 and the fork's #12, recorded under its new name.
    let fixture = fixture_with(REGISTRY_ORIGIN, REGISTRY_ORIGIN);
    fixture.depends_of(
        "feat/base",
        &["--on", "maintainer/demo#12", "--on", "acme/demo#12"],
    );

    // When / Then, with the forge up: `demo#12` names both, refused.
    fixture.refused(
        "feat/base",
        &["--forget", "demo#12"],
        &["demo#12 answers to maintainer/demo#12, origin:acme/demo#12"],
    );

    // When: the forge is down.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = fixture.knives(&["--text", "depends", "feat/base", "--forget", "demo#12"]);

    // Then: whether it names the fork's #12 too could not be asked, so
    // nothing is taken back (exit 3), never the upstream's alone.
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "nothing recorded: whether demo#12 also names origin:acme/demo#12 could not be asked \
             of the forge"
        ),
        "{stderr}"
    );
    assert_eq!(
        fixture.required("feat/base"),
        ["maintainer/demo#12", "origin:acme/demo#12"]
    );
}

#[test]
fn an_explicit_origin_alias_is_no_upstream_token() {
    // Given: feat/alpha requires the fork's #13, and another fork's upstream
    // is this fork's own repository under its old name, acme-old/demo.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#13"]);
    let registry = fixture.home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        text.replace(
            "https://forge.invalid/maintainer/sibling",
            "https://forge.invalid/acme-old/demo",
        ),
    )
    .expect("rewrite the registry");

    // When / Then: `origin:` names this fork's own repository, so the old
    // name takes the record back.
    fixture.depends(&["--forget", "origin:acme-old/demo#13"]);
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_member_token_under_the_former_name_takes_back_every_spelling() {
    for (origin, registered) in [(ORIGIN, ORIGIN), (ORIGIN, REGISTRY_ORIGIN)] {
        // Given: feat/alpha requires feat/base under both names of the fork's
        // repository.
        let fixture = fixture_with(origin, registered);
        fixture.depends(&["--on", "feat/base"]);
        fixture.rewrite_requirements(
            "origin:acme/demo:feat/base",
            "origin:acme-old/demo:feat/base,origin:acme/demo:feat/base",
        );

        // When / Then: the former name's token takes back both.
        fixture.depends(&["--forget", "origin:acme-old/demo:feat/base"]);
        assert_eq!(
            fixture.required("feat/alpha"),
            Vec::<String>::new(),
            "{registered}"
        );
    }
}

#[test]
fn a_correction_to_the_new_name_writes_the_new_name() {
    // Given: both on the new name; feat/alpha requires the fork's #13 under
    // its old name.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    fixture.depends(&["--on", "acme/demo#13"]);
    fixture.rewrite_requirements("origin:acme/demo#13", "origin:acme-old/demo#13");

    // When: the old spelling is forgotten and the new one named.
    let stdout = fixture.depends(&[
        "--forget",
        "origin:acme-old/demo#13",
        "--on",
        "acme/demo#13",
    ]);

    // Then: the list holds the new spelling.
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/alpha no longer requires origin:acme-old/demo#13; it requires \
         origin:acme/demo#13"
    );
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);
}

#[test]
fn a_registry_spelling_naming_another_live_repository_is_no_review_pull_request_here() {
    // Given: the checkout's origin names acme/demo; the registry names
    // another live repository, acme/other, which has #12 from acme's
    // feat/base.
    let fixture = fixture_with(ORIGIN, "https://forge.invalid/acme/other");

    // When / Then: acme/other#12 is refused, as `status` would never judge
    // it as this fork's.
    fixture.refused(
        "feat/alpha",
        &["--on", "acme/other#12"],
        &["acme/other is not demo's own repository: its checkout's origin names acme/demo"],
    );
}

#[test]
fn an_exact_sibling_member_token_takes_back_that_record_alone() {
    // Given: feat/alpha requires this fork's feat/base, and holds a record
    // naming the sibling's feat/base, as a hand edit could leave.
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);
    fixture.rewrite_requirements(
        "origin:acme/demo:feat/base",
        "origin:acme/demo:feat/base,origin:acme/sibling:feat/base",
    );

    // When / Then: the sibling's token takes back that record alone.
    fixture.depends(&["--forget", "origin:acme/sibling:feat/base"]);
    assert_eq!(
        fixture.required("feat/alpha"),
        ["origin:acme/demo:feat/base"]
    );
}

#[test]
fn depends_reads_the_checkouts_origin_as_status_and_track_do() {
    // Given: the checkout's `origin` names the fork's own repository,
    // acme/demo, and the registry names another live one, acme/other. The
    // fork's #12 and the upstream's #12 both exist.
    let fixture = fixture_with(ORIGIN, "https://forge.invalid/acme/other");

    // When / Then: `--on demo#12` asks the repository the checkout names, so
    // the number is ambiguous, and refused naming both.
    fixture.refused(
        "feat/alpha",
        &["--on", "demo#12"],
        &[
            "names two pull requests",
            "maintainer/demo#12",
            "acme/demo#12",
        ],
    );
}

/// Advance `branch` here with a commit adding `file`, and leave `@` empty on
/// top of it.
fn advance(lab: &Lab, branch: &str, file: &str, message: &str) {
    lab.jj_work(["new", "-r", branch, "-m", message]);
    std::fs::write(lab.work.join(file), format!("{message}\n")).expect("write the new work");
    lab.jj_work(["bookmark", "set", branch, "-r", "@"]);
    lab.jj_work(["new"]);
}

#[test]
fn an_old_recorded_dependent_commit_does_not_show_its_later_work_landed() {
    // Given: feat/dep requires feat/base, recorded at C; C's work lands
    // upstream; feat/dep then advances to D with more work, held also by
    // review/base. Its requirement is unmet while feat/dep is here.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/dep", "dep.txt", "dep\n");
    fixture.depends_of("feat/dep", &["--on", "feat/base"]);
    let recorded = lab::commit_at(lab, "feat/dep");
    lab.upstream_trunk_file("dep.txt", "dep\n");
    advance(
        lab,
        "feat/dep",
        "later-dep.txt",
        "later work that still requires base",
    );
    lab.jj_work(["bookmark", "create", "review/base", "-r", "feat/dep"]);
    let (_, control) = status(&fixture);
    assert!(
        details(&control, "unmet-dependency")
            .iter()
            .any(|detail| detail.starts_with("feat/dep requires ")),
        "{control}"
    );

    // When: feat/dep alone is deleted; D stays in review/base.
    lab.jj_work(["bookmark", "delete", "feat/dep"]);
    let (code, report) = status(&fixture);

    // Then: C's landing is not taken as feat/dep's: not silent, a problem
    // naming what could not be read.
    let problems = problems(&report);
    assert!(
        problems.iter().any(|problem| problem.starts_with(&format!(
            "cannot tell whether feat/dep still needs origin:acme/demo:feat/base: its branch is \
             gone here and on origin, and the newest commit the ledger records for it, {}, has \
             landed",
            recorded.short()
        ))),
        "{problems:?}\n{report}"
    );
    assert_eq!(code, Some(3), "{report}");
}

#[test]
fn a_deleted_dependent_is_judged_by_the_newest_commit_any_entry_records() {
    // Given: feat/dep requires feat/base, recorded at C, whose work lands;
    // feat/dep advances to D, and a note about it is written there; then
    // feat/dep is deleted.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/dep", "dep.txt", "dep\n");
    fixture.depends_of("feat/dep", &["--on", "feat/base"]);
    lab.upstream_trunk_file("dep.txt", "dep\n");
    advance(lab, "feat/dep", "later-dep.txt", "later work");
    let later = lab::commit_at(lab, "feat/dep");
    let note = fixture.knives(&["--text", "notch", "feat/dep", "-m", "advanced"]);
    assert!(note.status.success(), "{}", said(&note));
    lab.jj_work(["bookmark", "delete", "feat/dep"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: the note's commit, D, is the newest recorded, has not landed,
    // and so the requirement is checked, and unmet; a note says why.
    assert!(
        details(&report, "unmet-dependency").contains(
            &"feat/dep requires origin:acme/demo:feat/base, which has not landed (no-pr)"
                .to_owned()
        ),
        "{report}"
    );
    let notes: Vec<&str> = report["notes"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        notes.contains(
            &format!(
                "feat/dep is gone here and on origin; its requirements are checked because the \
             newest commit the ledger records for it, {}, has not landed",
                later.short()
            )
            .as_str()
        ),
        "{notes:?}"
    );
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn a_deleted_dependents_recorded_commit_outlives_a_later_forget() {
    // Given: feat/dep requires feat/base, recorded at C, not landed; feat/dep
    // is deleted, and then its list is corrected, which records no commit.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/dep", "dep.txt", "dep\n");
    fixture.depends_of("feat/dep", &["--on", "feat/base"]);
    lab.jj_work(["bookmark", "delete", "feat/dep"]);
    fixture.depends_of(
        "feat/dep",
        &["--forget", "feat/base", "--on", "acme/demo#13"],
    );

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: C is still its recorded commit, so the corrected list is checked.
    assert!(
        details(&report, "unmet-dependency")
            .iter()
            .any(|detail| detail.starts_with(
                "feat/dep requires origin:acme/demo#13 (head feat/base, open), which has not landed"
            )),
        "{report}"
    );
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn a_dependent_the_ledger_records_no_commit_for_is_reported_unknown() {
    // Given: feat/ghost, never a branch here, requires feat/base.
    let fixture = fixture();
    fixture.depends_of("feat/ghost", &["--on", "feat/base"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: not silent: a problem saying the ledger records no commit for it.
    assert!(
        problems(&report).iter().any(|problem| problem.starts_with(
            "cannot tell whether feat/ghost still needs origin:acme/demo:feat/base: its branch \
             is gone here and on origin, and the ledger records no commit for it; \
             origin:acme/demo:feat/base, which has not landed (no-pr). Forget these once its \
             work has landed or been dropped: `knives depends feat/ghost --forget feat/base`"
        )),
        "{report}"
    );
    assert_eq!(code, Some(3), "{report}");
}

#[test]
fn a_member_with_a_conflicted_local_bookmark_is_judged_on_origin_too() {
    // Given: feat/base, two commits, pushed and squash-merged upstream as
    // #40; feat/alpha requires it: met.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    let lab = &fixture.lab;
    advance(lab, "feat/base", "base2.txt", "second base commit");
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
    fixture.depends(&["--on", "feat/base"]);
    let (_, control) = status(&fixture);
    assert_eq!(
        details(&control, "unmet-dependency"),
        Vec::<String>::new(),
        "{control}"
    );

    // When: this checkout stops tracking feat/base, two concurrent
    // operations move it to two of its merged commits (a conflicted local
    // bookmark, both sides within what merged), and another clone advances
    // it on origin with new work, which the fetch brings in.
    lab.jj_work(["bookmark", "untrack", "feat/base", "--remote", "origin"]);
    let operation = lab.jj_work_output(["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    lab.jj_work([
        "bookmark",
        "set",
        "feat/base",
        "-r",
        "feat/base-",
        "--allow-backwards",
        "--ignore-working-copy",
    ]);
    lab.jj_work([
        "--at-op",
        operation.as_str(),
        "bookmark",
        "set",
        "feat/base",
        "-r",
        "feat/base--",
        "--allow-backwards",
        "--ignore-working-copy",
    ]);
    lab.advance_origin_branch("feat/base", "new work on origin\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    let conflicted = lab.jj_work_output([
        "bookmark",
        "list",
        "feat/base",
        "-T",
        "if(conflict, \"conflicted\", \"single\") ++ \"\\n\"",
    ]);
    assert!(conflicted.contains("conflicted"), "{conflicted}");
    let (code, report) = status(&fixture);

    // Then: with no single local tip, origin's copy is judged too, and its
    // new work has not landed.
    assert!(
        details(&report, "unmet-dependency")
            .iter()
            .any(|detail| detail.starts_with(
                "feat/alpha requires origin:acme/demo:feat/base, which has not landed \
             (feat/base@origin "
            )),
        "{report}"
    );
    assert_ne!(code, Some(0), "{report}");
}

// The input space of `depends --forget` and of `status`'s dependency check,
// one case per row of the tables in the pull request. Each table test
// collects every row that fails and reports them together.

/// The checkout's `origin` and the registry's `origin`: both on the
/// repository's former name (A), both on its new name (B), or the checkout
/// on the new one and the registry on the former (X).
type Config = (&'static str, &'static str);
const CONFIG_A: Config = (REGISTRY_ORIGIN, REGISTRY_ORIGIN);
const CONFIG_B: Config = (ORIGIN, ORIGIN);
const CONFIG_X: Config = (ORIGIN, REGISTRY_ORIGIN);

/// How the fixture's forge answers when the command under test runs.
#[derive(Clone, Copy, Debug)]
enum ForgeNow {
    /// Every repository answers.
    Up,
    /// Every call fails, as when the forge cannot be reached.
    Down,
    /// The fork's own repository, by either name, answers as one the token
    /// cannot see: "Could not resolve to a Repository".
    OwnUnseen,
    /// The fork's upstream answers as one the token cannot see.
    UpstreamUnseen,
    /// The fork's own repository answers by its new name; every call naming
    /// its former name fails as an outage does.
    FormerNameFails,
    /// The fork's own repository answers by its new name; its former name
    /// answers as one the token cannot see.
    FormerNameUnseen,
}

impl Fixture {
    /// Make the forge answer as `now` says.
    fn forge_now(&self, now: ForgeNow) {
        match now {
            ForgeNow::Down => install_failing_gh(self.shim.path(), &self.log),
            ForgeNow::Up => self.forge_as(|state| {
                state.hidden.clear();
                state.failing.clear();
            }),
            ForgeNow::OwnUnseen => self.forge_as(|state| {
                state.hidden = vec!["acme/demo", "acme-old/demo"];
                state.failing.clear();
            }),
            ForgeNow::UpstreamUnseen => self.forge_as(|state| {
                state.hidden = vec![KEPT_UNDER];
                state.failing.clear();
            }),
            ForgeNow::FormerNameFails => self.forge_as(|state| {
                state.hidden.clear();
                state.failing = vec!["acme-old/demo"];
            }),
            ForgeNow::FormerNameUnseen => self.forge_as(|state| {
                state.hidden = vec!["acme-old/demo"];
                state.failing.clear();
            }),
        }
    }

    /// Make `branch` require exactly `list`, as written: a list another
    /// machine, an older knives or a hand edit could leave, which this
    /// knives would not write itself.
    fn hold(&self, branch: &str, list: &[&str]) {
        let _ = self.depends_of(branch, &["--on", "feat/landed"]);
        let held = self.required(branch).join(",");
        let rewritten = self.rewrite_entries(
            Some(branch),
            &format!("value = \"{held}\""),
            &format!("value = \"{}\"", list.join(",")),
        );
        assert_eq!(rewritten, 1, "no one entry about {branch} states {held}");
    }
}

const U12: &str = "maintainer/demo#12";
const F12: &str = "origin:acme/demo#12";
const F13: &str = "origin:acme/demo#13";
const F13_OLD: &str = "origin:acme-old/demo#13";
const M: &str = "origin:acme/demo:feat/base";
const M_OLD: &str = "origin:acme-old/demo:feat/base";
const SIBLING_M: &str = "origin:acme/sibling:feat/base";
const GONE13: &str = "origin:gone/demo#13";
const OTHER13: &str = "origin:acme/other#13";
const U7: &str = "maintainer/demo#7";
const SIB12: &str = "origin:acme/sibling#12";
const OTHER_M: &str = "origin:acme/other:feat/base";

/// One row of the `--forget` table: feat/alpha holds `held` in `config`,
/// and `depends feat/alpha --forget <text>…` runs with the forge `now`.
struct ForgetRow {
    row: &'static str,
    config: Config,
    held: &'static [&'static str],
    now: ForgeNow,
    texts: &'static [&'static str],
    /// The exit code, and what feat/alpha requires afterwards.
    exit: i32,
    left: &'static [&'static str],
}

/// What a refusal's stderr must hold, by row: exit 2 says why; exit 3 says
/// what could not be asked and prints a command ([`printed_commands`]).
fn refusal_says(row: &str) -> &'static str {
    match row {
        "F4" | "F7" => "demo#12 answers to maintainer/demo#12, origin:acme/demo#12; name the one",
        "F5" | "F6" | "F47" => "If it does, it names two requirements.",
        "F37" | "F56" => {
            "nothing recorded: demo#13 answers to origin:acme-old/demo#13, origin:acme/demo#13, and \
             whether they are one requirement could not be asked of the forge"
        }
        "F29" => "which is sibling's own repository, not demo's",
        "F34" => "does not require origin:acme/demo#99",
        "F43" => "does not require feat/base",
        _ => "nothing recorded: ",
    }
}

/// The advice of a `--forget` refusal when the forge was down or failing.
const OUTAGE_ADVICE: &str = "Run it again when the forge answers, or take back";
/// The advice of a `--forget` refusal when a reason is the forge's "Could
/// not resolve to a Repository" (N104).
const UNRESOLVED_ADVICE: &str = "The forge could not resolve a repository it was asked about: it \
                                 is gone, the token cannot see it, or the forge is failing. Run \
                                 it again later, or check the repository, or take back";

/// What is wrong with an exit-3 `--forget` refusal's advice for the reasons
/// it prints, if anything (N107): every refusal site, `choices` included,
/// gives the advice its reasons call for, and not the other.
fn advice_wrong(stderr: &str) -> Option<String> {
    let (advice, not) = if stderr.contains("Could not resolve to a Repository") {
        (UNRESOLVED_ADVICE, OUTAGE_ADVICE)
    } else {
        (OUTAGE_ADVICE, UNRESOLVED_ADVICE)
    };
    (!stderr.contains(advice) || stderr.contains(not))
        .then(|| format!("the refusal does not advise `{advice}` alone: {stderr}"))
}

/// What the list holds after each command an exit-3 refusal prints, by
/// row, in any order, one list per command: every candidate record of the
/// text taken back, and nothing else; one requirement per command when the
/// text may name two.
fn recovered(row: &str) -> Vec<Vec<&'static str>> {
    match row {
        "F5" | "F6" => vec![vec![F12], vec![U12]],
        "F41" => vec![vec![U7]],
        "F45" => vec![vec!["origin:acme/demo#14"]],
        "F46" | "F57" => vec![vec![SIBLING_M]],
        "F47" => vec![vec![F12, SIB12], vec![U12, SIB12]],
        _ => vec![Vec::new()],
    }
}

/// Every `knives depends feat/alpha …` command a refusal prints, each as
/// arguments after `knives`.
fn printed_commands(stderr: &str) -> Vec<Vec<String>> {
    stderr
        .split('`')
        .filter(|part| part.starts_with("knives depends feat/alpha "))
        .map(|command| {
            command
                .split_whitespace()
                .skip(1)
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

/// What `held` leaves once `command` (arguments after `knives`) takes back
/// the records it names: each spelled as a record, or a member of this fork
/// named by its branch.
fn left_by(held: &[&str], command: &[String]) -> Vec<String> {
    let named: Vec<&str> = command
        .iter()
        .map(String::as_str)
        .filter(|word| *word != "--forget")
        .collect();
    let mut left: Vec<String> = held
        .iter()
        .filter(|record| {
            !named.contains(record)
                && !record.strip_prefix("origin:").is_some_and(|token| {
                    !token.starts_with("acme/sibling:")
                        && token
                            .split_once(':')
                            .is_some_and(|(_, branch)| named.contains(&branch))
                })
        })
        .map(|record| (*record).to_owned())
        .collect();
    left.sort();
    left
}

/// Run each of `rows`; every row whose exit, list or refusal differs, said.
/// A refusal writes nothing and says why. An exit-3 refusal prints one
/// command per list [`recovered`] gives; each, run on the list as it was,
/// with the forge as it is, leaves the list less exactly what it names, and
/// the lists the commands leave are the row's.
fn forget_rows(rows: &[ForgetRow]) -> Vec<String> {
    let mut failed = Vec::new();
    for row in rows {
        let fixture = fixture_with(row.config.0, row.config.1);
        fixture.hold("feat/alpha", row.held);
        fixture.forge_now(row.now);
        let before = fixture.entries().len();
        let mut args = vec!["--text", "depends", "feat/alpha"];
        for text in row.texts {
            args.extend(["--forget", text]);
        }
        let output = fixture.knives(&args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let mut left = fixture.required("feat/alpha");
        left.sort();
        let mut wanted: Vec<String> = row.left.iter().map(|text| (*text).to_owned()).collect();
        wanted.sort();
        let shown = format!(
            "{}: {:?} --forget {:?} with the forge {:?}",
            row.row, row.held, row.texts, row.now
        );
        if output.status.code() != Some(row.exit) || left != wanted {
            failed.push(format!(
                "{shown}: exit {:?}, left {left:?}; the table says exit {}, left {wanted:?}\n  {}",
                output.status.code(),
                row.exit,
                said(&output).replace('\n', "\n  ")
            ));
            continue;
        }
        if row.exit == 0 {
            continue;
        }
        if fixture.entries().len() != before || !stderr.contains(refusal_says(row.row)) {
            failed.push(format!(
                "{shown}: the refusal wrote an entry or does not say `{}`: {stderr}",
                refusal_says(row.row)
            ));
            continue;
        }
        if row.exit != 3 {
            continue;
        }
        if let Some(wrong) = advice_wrong(&stderr) {
            failed.push(format!("{shown}: {wrong}"));
            continue;
        }
        let commands = printed_commands(&stderr);
        if commands.len() != recovered(row.row).len() {
            failed.push(format!(
                "{shown}: the exit-3 refusal prints {} commands, not {}: {stderr}",
                commands.len(),
                recovered(row.row).len()
            ));
            continue;
        }
        let mut afters: Vec<Vec<String>> = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            // Each command runs on the list as the refusal left it.
            let fresh;
            let on = if index == 0 {
                &fixture
            } else {
                fresh = fixture_with(row.config.0, row.config.1);
                fresh.hold("feat/alpha", row.held);
                fresh.forge_now(row.now);
                &fresh
            };
            let mut run = vec!["--text"];
            run.extend(command.iter().map(String::as_str));
            let recovery = on.knives(&run);
            let mut still = on.required("feat/alpha");
            still.sort();
            let expected = left_by(row.held, command);
            if !recovery.status.success() || still != expected {
                failed.push(format!(
                    "{shown}: the printed command {command:?} left {still:?}, not \
                     {expected:?}\n  {}",
                    said(&recovery).replace('\n', "\n  ")
                ));
            }
            afters.push(still);
        }
        afters.sort();
        let mut wanted_after: Vec<Vec<String>> = recovered(row.row)
            .into_iter()
            .map(|list| {
                let mut list: Vec<String> = list.into_iter().map(str::to_owned).collect();
                list.sort();
                list
            })
            .collect();
        wanted_after.sort();
        if afters != wanted_after {
            failed.push(format!(
                "{shown}: the printed commands {commands:?} left {afters:?}, not \
                 {wanted_after:?}"
            ));
        }
    }
    failed
}

#[test]
fn forgetting_an_upstream_pull_request_follows_the_table() {
    let failed = forget_rows(&[
        ForgetRow {
            row: "F1",
            config: CONFIG_X,
            held: &[U12, F12],
            now: ForgeNow::Up,
            texts: &[U12],
            exit: 0,
            left: &[F12],
        },
        ForgetRow {
            row: "F2",
            config: CONFIG_X,
            held: &[U12, F12],
            now: ForgeNow::Down,
            texts: &[U12],
            exit: 0,
            left: &[F12],
        },
        ForgetRow {
            row: "F3",
            config: CONFIG_A,
            held: &[U12, F12],
            now: ForgeNow::OwnUnseen,
            texts: &[U12],
            exit: 0,
            left: &[F12],
        },
        ForgetRow {
            row: "F4",
            config: CONFIG_A,
            held: &[U12, F12],
            now: ForgeNow::Up,
            texts: &["demo#12"],
            exit: 2,
            left: &[U12, F12],
        },
        ForgetRow {
            row: "F5",
            config: CONFIG_A,
            held: &[U12, F12],
            now: ForgeNow::Down,
            texts: &["demo#12"],
            exit: 3,
            left: &[U12, F12],
        },
        ForgetRow {
            row: "F6",
            config: CONFIG_A,
            held: &[U12, F12],
            now: ForgeNow::OwnUnseen,
            texts: &["demo#12"],
            exit: 3,
            left: &[U12, F12],
        },
        ForgetRow {
            row: "F7",
            config: CONFIG_B,
            held: &[U12, F12],
            now: ForgeNow::Down,
            texts: &["demo#12"],
            exit: 2,
            left: &[U12, F12],
        },
        ForgetRow {
            row: "F8",
            config: CONFIG_A,
            held: &[U12, F12],
            now: ForgeNow::Down,
            texts: &[U12, F12],
            exit: 0,
            left: &[],
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn forgetting_a_review_pull_request_under_one_name_follows_the_table() {
    let failed = forget_rows(&[
        ForgetRow {
            row: "F9",
            config: CONFIG_X,
            held: &[F13],
            now: ForgeNow::Down,
            texts: &[F13],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F10",
            config: CONFIG_X,
            held: &[F13],
            now: ForgeNow::Down,
            texts: &["demo#13"],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F11",
            config: CONFIG_B,
            held: &[F13],
            now: ForgeNow::Up,
            texts: &["acme/demo#13"],
            exit: 0,
            left: &[],
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn forgetting_a_review_pull_request_under_two_names_follows_the_table() {
    let both: &[&str] = &[F13_OLD, F13];
    let failed = forget_rows(&[
        ForgetRow {
            row: "F12",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Up,
            texts: &[F13],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F13",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Down,
            texts: &[F13],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F14",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::OwnUnseen,
            texts: &[F13],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F15",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::FormerNameFails,
            texts: &[F13],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F16",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Up,
            texts: &[F13_OLD],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F17",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Down,
            texts: &[F13, F13_OLD],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F18",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::OwnUnseen,
            texts: &[F13, F13_OLD],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F19",
            config: CONFIG_A,
            held: both,
            now: ForgeNow::Up,
            texts: &["demo#13"],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F20",
            config: CONFIG_A,
            held: both,
            now: ForgeNow::Down,
            texts: &["demo#13"],
            exit: 3,
            left: both,
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn forgetting_a_member_branch_follows_the_table() {
    let both: &[&str] = &[M_OLD, M];
    let failed = forget_rows(&[
        // With the forge down, nothing tells whether the former name's
        // record is this fork's member: a bare name or the own token takes
        // nothing, and prints both records' tokens (N71).
        ForgetRow {
            row: "F21",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Down,
            texts: &["feat/base"],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F22",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Down,
            texts: &[M],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F23",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Up,
            texts: &[M_OLD],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F24",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Down,
            texts: &[M_OLD],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F25",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::OwnUnseen,
            texts: &[M_OLD],
            exit: 3,
            left: both,
        },
        ForgetRow {
            row: "F26",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Down,
            texts: &[M_OLD, M],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F27",
            config: CONFIG_A,
            held: both,
            now: ForgeNow::Down,
            texts: &[M],
            exit: 3,
            left: both,
        },
        // N86: a token whose repository differs from the record's only in
        // letter case spells that record, and takes every record of the
        // member the forge places with it.
        ForgetRow {
            row: "F55",
            config: CONFIG_B,
            held: both,
            now: ForgeNow::Up,
            texts: &["origin:ACME-OLD/demo:feat/base"],
            exit: 0,
            left: &[],
        },
        // N110: F46 (the beside-other-requirements table) with the forge
        // down, so the member refusal gives the outage advice.
        ForgetRow {
            row: "F57",
            config: CONFIG_X,
            held: &[M, SIBLING_M],
            now: ForgeNow::Down,
            texts: &["origin:gone/demo:feat/base"],
            exit: 3,
            left: &[M, SIBLING_M],
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn forgetting_a_record_on_another_repository_follows_the_table() {
    let failed = forget_rows(&[
        ForgetRow {
            row: "F28",
            config: CONFIG_X,
            held: &[SIBLING_M, M],
            now: ForgeNow::Down,
            texts: &[SIBLING_M],
            exit: 0,
            left: &[M],
        },
        ForgetRow {
            row: "F29",
            config: CONFIG_X,
            held: &[M],
            now: ForgeNow::Down,
            texts: &[SIBLING_M],
            exit: 2,
            left: &[M],
        },
        ForgetRow {
            row: "F30",
            config: CONFIG_B,
            held: &[F13, GONE13],
            now: ForgeNow::Up,
            texts: &[F13],
            exit: 3,
            left: &[F13, GONE13],
        },
        ForgetRow {
            row: "F31",
            config: CONFIG_B,
            held: &[F13, GONE13],
            now: ForgeNow::Up,
            texts: &[GONE13],
            exit: 3,
            left: &[F13, GONE13],
        },
        ForgetRow {
            row: "F32",
            config: CONFIG_B,
            held: &[F13, GONE13],
            now: ForgeNow::Up,
            texts: &[F13, GONE13],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F33",
            config: CONFIG_B,
            held: &[F13, GONE13],
            now: ForgeNow::Down,
            texts: &[F13, GONE13],
            exit: 0,
            left: &[],
        },
        ForgetRow {
            row: "F34",
            config: CONFIG_X,
            held: &[F13],
            now: ForgeNow::Down,
            texts: &["origin:acme/demo#99"],
            exit: 2,
            left: &[F13],
        },
        // T7: a member token on a live repository that is no fork's names
        // that record alone when the forge says the repository is another;
        // with the forge down, it might be this fork's former name.
        ForgetRow {
            row: "F51",
            config: CONFIG_X,
            held: &[OTHER_M, M],
            now: ForgeNow::Up,
            texts: &[OTHER_M],
            exit: 0,
            left: &[M],
        },
        ForgetRow {
            row: "F52",
            config: CONFIG_X,
            held: &[OTHER_M, M],
            now: ForgeNow::Down,
            texts: &[OTHER_M],
            exit: 3,
            left: &[OTHER_M, M],
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn forgetting_where_only_the_forge_could_decide_follows_the_table() {
    let three: &[&str] = &[F13_OLD, F13, OTHER13];
    let failed = forget_rows(&[
        // T5: the text names its one record only through the forge.
        ForgetRow {
            row: "F35",
            config: CONFIG_B,
            held: &[F13_OLD],
            now: ForgeNow::Down,
            texts: &[F13],
            exit: 3,
            left: &[F13_OLD],
        },
        ForgetRow {
            row: "F36",
            config: CONFIG_B,
            held: &[F13_OLD],
            now: ForgeNow::Up,
            texts: &[F13],
            exit: 0,
            left: &[],
        },
        // Sol's R8-N2: both spellings answer `demo#13` directly; whether
        // they are one requirement is the forge's to say.
        ForgetRow {
            row: "F37",
            config: CONFIG_X,
            held: &[F13_OLD, F13],
            now: ForgeNow::Down,
            texts: &["demo#13"],
            exit: 3,
            left: &[F13_OLD, F13],
        },
        ForgetRow {
            row: "F38",
            config: CONFIG_X,
            held: &[F13_OLD, F13],
            now: ForgeNow::Up,
            texts: &["demo#13"],
            exit: 0,
            left: &[],
        },
        // N107: F37 with the forge unable to resolve this fork's own
        // repository, so the refusal that the two may be one requirement
        // gives the unresolved advice.
        ForgetRow {
            row: "F56",
            config: CONFIG_X,
            held: &[F13_OLD, F13],
            now: ForgeNow::OwnUnseen,
            texts: &["demo#13"],
            exit: 3,
            left: &[F13_OLD, F13],
        },
        // N51(b): a third record of the number, on a repository the forge
        // names apart; the former name's lookup fails.
        ForgetRow {
            row: "F39",
            config: CONFIG_B,
            held: three,
            now: ForgeNow::FormerNameFails,
            texts: &[F13],
            exit: 3,
            left: three,
        },
        ForgetRow {
            row: "F40",
            config: CONFIG_B,
            held: three,
            now: ForgeNow::Up,
            texts: &[F13],
            exit: 0,
            left: &[OTHER13],
        },
        // T8: the same in config A, where nothing here spells acme/demo and
        // this fork's member reaches the forget only through the forge
        // placing acme/demo as the fork's own.
        ForgetRow {
            row: "F53",
            config: CONFIG_A,
            held: &[OTHER_M, M],
            now: ForgeNow::Up,
            texts: &[OTHER_M],
            exit: 0,
            left: &[M],
        },
        ForgetRow {
            row: "F54",
            config: CONFIG_A,
            held: &[OTHER_M, M],
            now: ForgeNow::Down,
            texts: &[OTHER_M],
            exit: 3,
            left: &[OTHER_M, M],
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn forgetting_beside_other_requirements_follows_the_table() {
    let failed = forget_rows(&[
        // N65: a requirement no text names stays, whatever the printed
        // command takes.
        ForgetRow {
            row: "F41",
            config: CONFIG_B,
            held: &[F13_OLD, F13, U7],
            now: ForgeNow::Down,
            texts: &[F13],
            exit: 3,
            left: &[F13_OLD, F13, U7],
        },
        // N59: an upstream record of a fork the registry does not keep,
        // spelled exactly, beside a review pull request of its number.
        ForgetRow {
            row: "F42",
            config: CONFIG_B,
            held: &["nobody/else#13", F13],
            now: ForgeNow::Up,
            texts: &["nobody/else#13"],
            exit: 0,
            left: &[F13],
        },
        // N61(a): a bare branch name is this fork's member, never another
        // fork's branch.
        ForgetRow {
            row: "F43",
            config: CONFIG_X,
            held: &[SIBLING_M],
            now: ForgeNow::Down,
            texts: &["feat/base"],
            exit: 2,
            left: &[SIBLING_M],
        },
        // N60: the upstream's own spelling, beside a member branch named so,
        // names the upstream pull request, as `--on` reads it.
        ForgetRow {
            row: "F44",
            config: CONFIG_X,
            held: &[U12, "origin:acme/demo:maintainer/demo#12"],
            now: ForgeNow::Down,
            texts: &[U12],
            exit: 0,
            left: &["origin:acme/demo:maintainer/demo#12"],
        },
        // N75: a review pull request of another number stays, whatever the
        // printed command takes.
        ForgetRow {
            row: "F45",
            config: CONFIG_B,
            held: &[F13_OLD, F13, "origin:acme/demo#14"],
            now: ForgeNow::Down,
            texts: &[F13],
            exit: 3,
            left: &[F13_OLD, F13, "origin:acme/demo#14"],
        },
        // N69: another fork's branch of the member's name is no record of
        // it: the printed command leaves it.
        ForgetRow {
            row: "F46",
            config: CONFIG_X,
            held: &[M, SIBLING_M],
            now: ForgeNow::Up,
            texts: &["origin:gone/demo:feat/base"],
            exit: 3,
            left: &[M, SIBLING_M],
        },
        // N70: each choice takes one requirement, every record of it; a
        // review pull request on another fork's repository is neither.
        ForgetRow {
            row: "F47",
            config: CONFIG_A,
            held: &[U12, F12, SIB12],
            now: ForgeNow::Down,
            texts: &["demo#12"],
            exit: 3,
            left: &[U12, F12, SIB12],
        },
        // N71: a member record on a live repository that is neither a
        // spelling here nor a registry fork's is no record of this fork's
        // member when the forge names it apart, and may be one when it
        // cannot be asked.
        ForgetRow {
            row: "F48",
            config: CONFIG_X,
            held: &[OTHER_M, M],
            now: ForgeNow::Up,
            texts: &["feat/base"],
            exit: 0,
            left: &[OTHER_M],
        },
        ForgetRow {
            row: "F49",
            config: CONFIG_X,
            held: &[OTHER_M, M],
            now: ForgeNow::Down,
            texts: &["feat/base"],
            exit: 3,
            left: &[OTHER_M, M],
        },
        // Sol's R10-N1 and N73: a text spelled exactly as a held upstream
        // record names it alone, beside a member branch named so, whether or
        // not the registry knows its repository.
        ForgetRow {
            row: "F50",
            config: CONFIG_X,
            held: &["nobody/else#13", "origin:acme/demo:nobody/else#13"],
            now: ForgeNow::Down,
            texts: &["nobody/else#13"],
            exit: 0,
            left: &["origin:acme/demo:nobody/else#13"],
        },
    ]);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn naming_a_member_offline_beside_another_spelling_says_it_writes_a_second_one() {
    // Given: config A, where nothing here spells acme/demo; feat/alpha holds
    // feat/base as a peer machine spells it, and the forge is down.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
    fixture.hold("feat/alpha", &[M]);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: feat/base is named.
    let output = fixture.depends(&["--on", "feat/base"]);

    // Then: this checkout's spelling is written beside the peer's, and the
    // output says so (N79), without calling acme/demo a former name: it is
    // the repository's current name (N84).
    let mut held = fixture.required("feat/alpha");
    held.sort();
    assert_eq!(held, [M_OLD, M], "{output}");
    assert!(
        output.contains(&format!(
            "{M_OLD} is recorded beside {M}: whether acme/demo is another name of demo's own \
             repository could not be asked of the forge; if it is, they are one requirement under \
             two spellings"
        )),
        "{output}"
    );
}

#[test]
fn naming_a_member_the_forge_places_reuses_its_record_and_says_nothing_more() {
    // Given: config A; feat/alpha holds feat/base as a peer machine spells
    // it, under the repository's current name; the forge answers.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
    fixture.hold("feat/alpha", &[M]);

    // When: feat/base is named.
    let output = fixture.depends(&["--on", "feat/base"]);

    // Then: the held record is reused, and no line speaks of two spellings
    // (N85).
    assert_eq!(fixture.required("feat/alpha"), [M], "{output}");
    assert!(!output.contains("is recorded beside"), "{output}");
}

#[test]
fn naming_a_member_beside_a_record_on_another_repository_records_this_forks_member() {
    // Given: feat/alpha holds a member record on acme/other, a live
    // repository that is not this fork's and no registry fork's.
    let fixture = fixture();
    fixture.hold("feat/alpha", &["origin:acme/other:feat/landed"]);

    // When: this fork's feat/landed is named.
    let output = fixture.depends(&["--on", "feat/landed"]);

    // Then: it is recorded beside the other record (Sol's R10-N2).
    let mut held = fixture.required("feat/alpha");
    held.sort();
    assert_eq!(
        held,
        [
            "origin:acme/demo:feat/landed",
            "origin:acme/other:feat/landed"
        ],
        "{output}"
    );
    // And: the forge named acme/other as another repository, so no line
    // says the two may be one requirement (N84).
    assert!(!output.contains("is recorded beside"), "{output}");
}

#[test]
fn a_review_pull_request_of_a_fork_of_this_fork_is_this_forks_review_pull_request() {
    // Given: the registry holds a fork of this fork, whose upstream is this
    // fork's own repository.
    let fixture = fixture();
    let registry = fixture.home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!(
            "{text}[repos.child]\nupstream = \"{ORIGIN}\"\n\
             origin = \"https://forge.invalid/acme/child\"\n"
        ),
    )
    .expect("rewrite the registry");

    // When: #13 is named by this fork's own repository.
    let output = fixture.depends(&["--on", "acme/demo#13"]);

    // Then: it is this fork's review pull request #13 (N74).
    assert_eq!(
        fixture.required("feat/alpha"),
        ["origin:acme/demo#13"],
        "{output}"
    );
}

#[test]
fn a_review_pull_request_under_the_forks_new_name_is_its_own_beside_a_fork_of_this_fork() {
    // Given: config A, where nothing here spells acme/demo, the fork's
    // current name; the registry holds a fork of this fork whose upstream is
    // acme/demo.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
    register_a_fork_of_this_fork(&fixture);

    // When: #13 is named by the fork's current name.
    let output = fixture.depends(&["--on", "acme/demo#13"]);

    // Then: the forge names this fork's repository acme/demo, so it is this
    // fork's review pull request #13, not the child's upstream pull request
    // (N83).
    let held = fixture.required("feat/alpha");
    assert_eq!(held.len(), 1, "{output}");
    assert!(
        held[0].starts_with("origin:") && held[0].ends_with("/demo#13"),
        "{held:?}\n{output}"
    );
}

/// Add to the fixture's registry `child`, a fork whose upstream is
/// acme/demo, the fork's current name, and whose own repository is
/// acme/child.
fn register_a_fork_of_this_fork(fixture: &Fixture) {
    register(fixture, "child", "acme/demo", "acme/child");
}

/// Add to the fixture's registry the fork `key`, whose upstream is the
/// repository `upstream` and whose own repository is `origin`.
fn register(fixture: &Fixture, key: &str, upstream: &str, origin: &str) {
    register_raw(
        fixture,
        key,
        &format!("https://forge.invalid/{upstream}"),
        &format!("https://forge.invalid/{origin}"),
    );
}

/// Add to the fixture's registry the fork `key` with its `upstream` and
/// `origin` written as given: a forge URL or a path.
fn register_raw(fixture: &Fixture, key: &str, upstream: &str, origin: &str) {
    let registry = fixture.home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!("{text}[repos.{key}]\nupstream = \"{upstream}\"\norigin = \"{origin}\"\n"),
    )
    .expect("rewrite the registry");
}

/// `--on <text>` for feat/alpha, with what it says.
fn on(fixture: &Fixture, text: &str) -> std::process::Output {
    fixture.knives(&["--text", "depends", "feat/alpha", "--on", text])
}

/// Assert `output` refused (exit 3) because only the forge can tell whether
/// `repo`, the upstream of `key`, is this fork's own repository, and that
/// nothing was recorded.
fn refused_for_the_forge(fixture: &Fixture, output: &std::process::Output, repo: &str, key: &str) {
    assert_eq!(output.status.code(), Some(3), "{}", said(output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "nothing recorded: {repo} in {repo}#"
        )) && stderr.contains(&format!(
            "is the upstream of {key}, and only the forge can tell whether it is also demo's own \
             repository under another name"
        )) && stderr.contains("Run it again when the forge answers"),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn offline_a_pull_request_of_a_fork_of_this_forks_upstream_is_refused() {
    // Given: config A, the child fork registered, and the forge down.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
    register_a_fork_of_this_fork(&fixture);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: #13 is named by acme/demo, the child's upstream, which may be
    // this fork's own repository under its current name.
    let output = on(&fixture, "acme/demo#13");

    // Then: nothing is recorded (exit 3), naming the child (N83).
    refused_for_the_forge(&fixture, &output, "acme/demo", "child");
}

#[test]
fn offline_a_third_partys_fork_of_this_fork_is_refused() {
    // Given: config A; the registry holds this fork and the child, a fork of
    // acme/demo owned by bob, and no fork owned by acme; the forge is down.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
    let registry = fixture.home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    let demo = text
        .split("[repos.")
        .find(|section| section.starts_with("demo]"))
        .unwrap_or_else(|| panic!("no demo section in {text}"));
    std::fs::write(&registry, format!("[repos.{demo}")).expect("rewrite the registry");
    register(&fixture, "child", "acme/demo", "bob/child");
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When / Then: as for any other fork's upstream, nothing is recorded
    // (N87).
    let output = on(&fixture, "acme/demo#13");
    refused_for_the_forge(&fixture, &output, "acme/demo", "child");
}

/// The fixture's forge after this fork's repository moved from acme-old to
/// newco, an owner no registered fork has: its review pull request #15,
/// whose head is this fork's feat/base, merged into a review base.
fn moved_to_newco(fixture: &Fixture) {
    let head = lab::commit_at(&fixture.lab, "feat/base")
        .as_str()
        .to_owned();
    let pulls = format!(
        "[{}]",
        pull_record_with_fields(
            15,
            "MERGED",
            "feat/base",
            &format!(
                ",\"headRefOid\":\"{head}\",\"headRepositoryOwner\":{{\"login\":\"newco\"}},\
                 \"baseRefName\":\"review/base\""
            ),
        )
    );
    install_repository_gh_failing(
        fixture.shim.path(),
        &[
            Repository {
                named: KEPT_UNDER,
                canonical: KEPT_UNDER,
                pulls: "[]",
            },
            Repository {
                named: "acme-old/demo",
                canonical: "newco/demo",
                pulls: "[]",
            },
            Repository {
                named: "newco/demo",
                canonical: "newco/demo",
                pulls: &pulls,
            },
            Repository {
                named: "maintainer/sibling",
                canonical: "maintainer/sibling",
                pulls: "[]",
            },
            Repository {
                named: "acme/sibling",
                canonical: "acme/sibling",
                pulls: "[]",
            },
        ],
        &[],
        &fixture.log,
    );
}

#[test]
fn a_pull_request_of_this_fork_after_a_transfer_is_never_another_forks_upstream_one() {
    // Given: config A; this fork's repository moved to newco/demo, and a
    // third party's fork of it, child, is registered with newco/demo as its
    // upstream.
    for forge_up in [true, false] {
        let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
        register(&fixture, "child", "newco/demo", "childco/demo");
        if forge_up {
            moved_to_newco(&fixture);
        } else {
            install_failing_gh(fixture.shim.path(), &fixture.log);
        }

        // When: #15 is named by the new name.
        let output = on(&fixture, "newco/demo#15");

        // Then: with the forge up, it is this fork's review pull request;
        // with the forge down, nothing is recorded, so no merge into a
        // review base can read as met (Sol's R13-B1).
        if forge_up {
            assert!(output.status.success(), "{}", said(&output));
            assert_eq!(fixture.required("feat/alpha"), ["origin:newco/demo#15"]);
        } else {
            refused_for_the_forge(&fixture, &output, "newco/demo", "child");
        }
    }
}

#[test]
fn offline_only_this_forks_registered_upstream_records_whatever_else_is_registered() {
    // Given: the forge down; with and without `tool`, an unrelated fork of
    // outsider/tool kept under maintainer, who also owns the sibling's
    // upstream.
    for with_tool in [false, true] {
        let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
        if with_tool {
            register(&fixture, "tool", "outsider/tool", "maintainer/tool");
        }
        install_failing_gh(fixture.shim.path(), &fixture.log);

        // When / Then: this fork's registered upstream's pull request is
        // recorded with no forge call.
        let output = on(&fixture, "maintainer/demo#7");
        assert!(output.status.success(), "{}", said(&output));
        assert_eq!(fixture.required("feat/alpha"), ["maintainer/demo#7"]);

        // And: another fork's upstream is refused, whatever else is
        // registered: only the forge can tell it from this fork's own
        // repository under another name (Sol's R13-B2).
        let fresh = fixture_with(CONFIG_A.0, CONFIG_A.1);
        if with_tool {
            register(&fresh, "tool", "outsider/tool", "maintainer/tool");
        }
        install_failing_gh(fresh.shim.path(), &fresh.log);
        let output = on(&fresh, "maintainer/sibling#51");
        refused_for_the_forge(&fresh, &output, "maintainer/sibling", "sibling");
    }
}

#[test]
fn online_another_forks_upstream_pull_request_is_recorded() {
    // Given: the forge answers.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);

    // When / Then: the sibling's upstream pull request is recorded as one.
    let output = on(&fixture, "maintainer/sibling#51");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["maintainer/sibling#51"]);
}

#[test]
fn online_a_fork_of_this_fork_under_its_former_name_is_its_own() {
    // Given: config B; the registry holds a fork of this fork whose upstream
    // spells this fork's former name, acme-old/demo; the forge answers.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register(&fixture, "child", "acme-old/demo", "acme/child");

    // When: #13 is named by the former name.
    let output = on(&fixture, "acme-old/demo#13");

    // Then: the forge redirects acme-old/demo to this fork's repository, so
    // it is this fork's review pull request (N88).
    let held = fixture.required("feat/alpha");
    assert_eq!(held.len(), 1, "{}", said(&output));
    assert!(
        held[0].starts_with("origin:") && held[0].ends_with("/demo#13"),
        "{held:?}\n{}",
        said(&output)
    );
}

#[test]
fn the_registry_key_of_a_fork_of_this_fork_names_this_forks_review_pull_request() {
    // Given: config B; the registry holds `child`, a fork of this fork whose
    // upstream is acme/demo, this fork's own repository; the forge answers.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register(&fixture, "child", "acme/demo", "acme/other");

    // When: #13 is named by the child's registry key.
    let output = on(&fixture, "child#13");

    // Then: it is this fork's review pull request, as `--on acme/demo#13`
    // records it, not an upstream pull request kept under the child (N93).
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);

    // And: the same spelling takes it back (N96).
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", "child#13"]);
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());

    // And: with the forge down, the key spelling is refused like the
    // `<owner>/<name>` one, and nothing is recorded.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = on(&fixture, "child#13");
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(
            "nothing recorded: acme/demo in child#13 is the upstream of child, and only the forge \
             can tell whether it is also demo's own repository under another name"
        ) && stderr.contains("Run it again when the forge answers"),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_failed_redirect_lookup_is_refused_never_read_as_another_repository() {
    // Given: config B; a fork of this fork registered under this fork's
    // former name; this fork's own name answers, and the former name's
    // lookup fails as an outage does.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register(&fixture, "child", "acme-old/demo", "acme/child");
    fixture.forge_now(ForgeNow::FormerNameFails);

    // When / Then: nothing is recorded, and the refusal says to run it again
    // (N94).
    let output = on(&fixture, "acme-old/demo#13");
    refused_for_the_forge(&fixture, &output, "acme-old/demo", "child");
}

#[test]
fn a_repository_the_forge_cannot_resolve_is_named_and_an_outage_is_retried() {
    // Given: `tool`, a fork whose upstream outsider/tool the forge cannot
    // resolve.
    let fixture = fixture();
    register(&fixture, "tool", "outsider/tool", "maintainer/tool");

    // When: one of its pull requests is named with the forge up.
    let output = on(&fixture, "outsider/tool#5");

    // Then: nothing is recorded, and the refusal names the repository the
    // forge could not resolve, and why that may be (N95, N101).
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "The forge could not resolve outsider/tool: it is gone, the token cannot see it, or \
             the forge is failing. Run it again later, or check the repository"
        ) && !stderr.contains("Run it again when the forge answers"),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());

    // And: with the forge down, the refusal says to run it again.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = on(&fixture, "outsider/tool#5");
    refused_for_the_forge(&fixture, &output, "outsider/tool", "tool");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("could not resolve"),
        "{}",
        said(&output)
    );
}

#[test]
fn forgetting_by_a_fork_of_this_forks_former_name_takes_this_forks_review_pull_request() {
    // Given: config B; a fork of this fork registered under this fork's
    // former name; feat/alpha requires this fork's #13, named so.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register(&fixture, "child", "acme-old/demo", "acme/child");
    let output = on(&fixture, "acme-old/demo#13");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);

    // When: it is forgotten by the same spelling with the forge down.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let forget = [
        "--text",
        "depends",
        "feat/alpha",
        "--forget",
        "acme-old/demo#13",
    ];
    let output = fixture.knives(&forget);

    // Then: only the forge can say the spelling is this fork's: nothing is
    // taken back (exit 3).
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);

    // And: with the forge up, `--forget` reads it as `--on` did (N96).
    fixture.forge_now(ForgeNow::Up);
    let output = fixture.knives(&forget);
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn forgetting_by_a_fork_of_this_forks_key_offline_takes_nothing_but_by_a_printed_command() {
    // Given: config B; `child`, a fork of this fork, registered; feat/alpha
    // holds #13 as the child's upstream pull request, as round 14 wrote it,
    // and as this fork's review pull request; the forge is down.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register(&fixture, "child", "acme/demo", "acme/other");
    fixture.hold("feat/alpha", &["acme/demo#13", F13]);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: #13 is forgotten by the child's registry key.
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", "child#13"]);

    // Then: only the forge can say whether the key names this fork's review
    // pull request too, so nothing is taken back (exit 3, N99), and the
    // refusal prints a command for each requirement.
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["acme/demo#13", F13]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Run it again when the forge answers")
            && stderr.contains("`knives depends feat/alpha --forget acme/demo#13`")
            && stderr.contains(&format!("`knives depends feat/alpha --forget {F13}`")),
        "{stderr}"
    );

    // And: the printed command for this fork's review pull request takes it
    // alone, with the forge still down.
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", F13]);
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["acme/demo#13"]);
}

#[test]
fn another_forks_key_is_refused_when_the_forge_cannot_resolve_this_forks_own_repository() {
    // Given: the sibling fork registered, and a forge that answers but
    // cannot resolve this fork's own repository, acme/demo.
    let fixture = fixture();
    fixture.forge_now(ForgeNow::OwnUnseen);

    // When: the sibling's #5 is named by its registry key.
    let output = on(&fixture, "sibling#5");

    // Then: whether the sibling's upstream is this fork's repository under
    // another name needs this fork's own name, which the forge could not
    // resolve: nothing is recorded (exit 3), and the refusal names this
    // fork's repository, not the sibling's (N100).
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(
            "nothing recorded: maintainer/sibling in sibling#5 is the upstream of sibling, and \
             only the forge can tell whether it is also demo's own repository under another name"
        ) && stderr.contains(
            "The forge could not resolve acme/demo: it is gone, the token cannot see it, or the \
             forge is failing. Run it again later, or check the repository"
        ),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_forget_the_forge_could_not_resolve_says_so_and_an_outage_is_retried() {
    // Given: `tool`, a fork whose upstream outsider/tool the forge cannot
    // resolve; feat/alpha requires this fork's #5.
    let fixture = fixture();
    register(&fixture, "tool", "outsider/tool", "maintainer/tool");
    fixture.hold("feat/alpha", &["origin:acme/demo#5"]);
    let forget = [
        "--text",
        "depends",
        "feat/alpha",
        "--forget",
        "outsider/tool#5",
    ];

    // When: tool's #5 is forgotten with the forge up.
    let output = fixture.knives(&forget);

    // Then: nothing is taken back (exit 3), and the refusal says the forge
    // could not resolve a repository, as `--on` does (N104).
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "The forge could not resolve a repository it was asked about: it is gone, the token \
             cannot see it, or the forge is failing. Run it again later, or check the \
             repository, or take back"
        ) && !stderr.contains("Run it again when the forge answers"),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#5"]);

    // And: with the forge down, the refusal says to run it again.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = fixture.knives(&forget);
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Run it again when the forge answers")
            && !stderr.contains("could not resolve"),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#5"]);
}

/// Add to the fixture's registry `local`, a fork whose upstream is a
/// filesystem path, so its upstream name is its registry key.
fn register_local(fixture: &Fixture) {
    register_raw(
        fixture,
        "local",
        &fixture.home.path().display().to_string(),
        "https://forge.invalid/acme/local",
    );
}

#[test]
fn a_local_upstream_forks_key_in_another_case_is_never_asked_of_the_forge() {
    // Given: `local`, a fork whose upstream is a filesystem path; feat/alpha
    // requires this fork's #12.
    let fixture = fixture();
    register_local(&fixture);
    fixture.hold("feat/alpha", &[F12]);

    // When: `Local#12` is forgotten with the forge up.
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", "Local#12"]);

    // Then: a path names no forge repository, so it is never this fork's
    // repository under another name: feat/alpha does not require it (exit
    // 2, N103), and this fork's #12 stays.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), [F12]);
}

#[test]
fn a_local_upstream_fork_is_named_on_by_its_key_exactly() {
    // Given: `local`, a fork whose upstream is a filesystem path.
    let fixture = fixture();
    register_local(&fixture);

    // When: its #12 is named by its key in another letter case, with the
    // forge up.
    let output = on(&fixture, "Local#12");

    // Then: nothing is recorded, and the refusal is a usage error that names
    // the key, never a forge question a rerun could not answer (exit 2,
    // N105).
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        "Local in Local#12 is no registry key: local's upstream is no forge repository, so it is \
         named by its key exactly: `--on local#12`"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());

    // And: the key as the refusal spells it records the pull request.
    let output = on(&fixture, "local#12");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["local#12"]);
}

#[test]
fn this_forks_path_upstream_is_named_by_its_key_in_any_letter_case() {
    // Given: this fork's upstream is a filesystem path, so its upstream name
    // is its registry key, `demo`.
    let fixture = fixture_on("", ORIGIN, ORIGIN);

    // When: its #12 is named by that key in another letter case.
    let output = on(&fixture, "DEMO#12");

    // Then: it is recorded under the key, with no forge call: the exact-key
    // refusal is for another fork's path upstream only (N108). The fixture
    // reads the ledger kept under `demo`, as the code writes it.
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["demo#12"]);
    assert_eq!(fixture.forge_calls(), "");

    // And: a review pull request, which only the forge can place, is a
    // forge call the log records.
    let output = on(&fixture, "acme/demo#13");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(
        fixture.required("feat/alpha"),
        ["demo#12", "origin:acme/demo#13"]
    );
    assert_ne!(fixture.forge_calls(), "");
}

#[test]
fn offline_what_needs_no_forge_question_is_recorded() {
    // Given: this fork's `origin`, in the checkout and the registry, is a
    // local path; `local`, a fork whose upstream is a path; the forge down.
    let fixture = fixture_with("", "");
    register_local(&fixture);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When / Then: another fork's upstream named by its `<owner>/<name>` is
    // recorded, and so is the path-upstream fork's exact key, with no forge
    // call: a local `origin` is never another fork's upstream under another
    // name, and a path upstream has no second repository (N109).
    let output = on(&fixture, "maintainer/sibling#51");
    assert!(output.status.success(), "{}", said(&output));
    let output = on(&fixture, "local#12");
    assert!(output.status.success(), "{}", said(&output));
    let both = ["local#12", "maintainer/sibling#51"];
    assert_eq!(fixture.required("feat/alpha"), both);
    assert_eq!(fixture.forge_calls(), "");

    // And: the sibling's registry key still asks the forge which of its two
    // repositories has #51: refused (exit 3), nothing added, and the
    // question is in the log.
    let output = on(&fixture, "sibling#51");
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), both);
    assert!(
        fixture.forge_calls().contains("repo view acme/sibling"),
        "{}",
        fixture.forge_calls()
    );
}

#[test]
fn offline_a_registry_key_with_one_forge_repository_is_recorded() {
    // Given: this fork's own repository is local, in the checkout and the
    // registry; `tool`, a fork whose upstream is on the forge and whose own
    // repository is a local path; the forge down.
    let fixture = fixture_with("", "");
    register_raw(
        &fixture,
        "tool",
        "https://forge.invalid/maintainer/tool",
        &fixture.home.path().display().to_string(),
    );
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When / Then: `tool#5`, and this fork's own `demo#12`, each name a fork
    // whose only forge repository is its upstream, and this fork's own
    // repository is no forge repository: each is that upstream's pull
    // request, recorded with no forge call (N114).
    let output = on(&fixture, "tool#5");
    assert!(output.status.success(), "{}", said(&output));
    let output = on(&fixture, "demo#12");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(
        fixture.required("feat/alpha"),
        ["maintainer/demo#12", "maintainer/tool#5"]
    );
    assert_eq!(fixture.forge_calls(), "");
}

#[test]
fn offline_an_upstream_this_forks_registry_origin_spells_is_refused() {
    // Given: this fork's registry `origin` spells its upstream,
    // maintainer/demo, and its checkout's `origin` is acme/demo; the forge
    // down.
    let fixture = fixture_with(ORIGIN, UPSTREAM);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: the upstream's #12 is named by its `<owner>/<name>`.
    let output = on(&fixture, "maintainer/demo#12");

    // Then: the name spells this fork's own repository first, which only
    // the forge can place: nothing is recorded (exit 3), and the forge was
    // asked (N112).
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
    assert_ne!(fixture.forge_calls(), "");

    // And: with the forge up, that spelling is refused as no repository of
    // this fork's, since the checkout's `origin` names acme/demo (N117,
    // N126) ...
    fixture.forge_now(ForgeNow::Up);
    let output = on(&fixture, "maintainer/demo#13");
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("maintainer/demo is not demo's own repository"),
        "{}",
        said(&output)
    );
    // ... while this fork's key is read as usual against the checkout's
    // repository: a number only the upstream has is the upstream's (N123).
    let output = on(&fixture, "demo#7");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["maintainer/demo#7"]);

    // And: a number both have is refused without offering the upstream's
    // spelling, which would be refused too, and the spelling it offers
    // records (N123).
    let depends =
        |args: &[&str]| fixture.knives(&[&["--text", "depends", "feat/base"], args].concat());
    let output = depends(&["--on", "demo#12"]);
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the review pull request is named `--on acme/demo#12`")
            && stderr.contains("the upstream's #12 has no spelling here")
            && !stderr.contains("`--on maintainer/demo#12`"),
        "{stderr}"
    );
    let output = depends(&["--on", "acme/demo#12"]);
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/base"), ["origin:acme/demo#12"]);

    // And: when the registry `origin` is this fork's own repository, the
    // same text records offline, with no forge call.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = on(&fixture, "maintainer/demo#12");
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["maintainer/demo#12"]);
    assert_eq!(fixture.forge_calls(), "");
}

#[test]
fn offline_an_upstream_this_forks_checkout_origin_spells_is_refused() {
    // Given: this fork's checkout `origin` spells its upstream,
    // maintainer/demo, and its registry `origin` is acme/demo; the forge
    // down.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    lab::git_output(&fixture.lab.work, ["remote", "set-url", "origin", UPSTREAM]);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: the upstream's #12 is named by its `<owner>/<name>`.
    let output = on(&fixture, "maintainer/demo#12");

    // Then: as for the registry's `origin`, the name spells this fork's own
    // repository first: nothing is recorded (exit 3), and the forge was
    // asked (N121).
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
    assert_ne!(fixture.forge_calls(), "");
}

#[test]
fn with_a_path_checkout_origin_the_registry_origin_spelling_the_upstream_is_this_forks() {
    // Given: this fork's checkout `origin` is a path, so its own repository
    // is its registry `origin`, which spells its upstream, maintainer/demo;
    // the forge answers.
    let fixture = fixture_with("", UPSTREAM);

    // When: #12 and #13 are named by this fork's key.
    for text in ["demo#12", "demo#13"] {
        let output = on(&fixture, text);
        assert!(output.status.success(), "{text}: {}", said(&output));
    }

    // Then: each is the upstream's number, with no lookup of it: this fork's
    // own repository spells the upstream, as the `--on` help says (N128),
    // never a refusal of a number both repositories have.
    assert_eq!(
        fixture.required("feat/alpha"),
        ["maintainer/demo#12", "maintainer/demo#13"]
    );
}

#[test]
fn a_fork_of_this_fork_with_a_local_origin_names_this_forks_review_pull_request() {
    // Given: config B; `child`, a fork of this fork (its upstream is
    // acme/demo, this fork's repository) whose own repository is a local
    // path; the forge answers.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register_raw(
        &fixture,
        "child",
        ORIGIN,
        &fixture.home.path().display().to_string(),
    );

    // When: #13 is named by the child's registry key.
    let output = on(&fixture, "child#13");

    // Then: it is this fork's review pull request: the child's one forge
    // repository skips only the number lookup, never the question whether
    // its upstream is this fork's repository (N116).
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);

    // And: with the forge down, only the forge can answer that question:
    // nothing is recorded (exit 3), never the child's upstream pull request
    // (N124).
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    register_raw(
        &fixture,
        "child",
        ORIGIN,
        &fixture.home.path().display().to_string(),
    );
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = on(&fixture, "child#13");
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_fork_of_this_fork_is_this_forks_beside_a_local_checkout_origin() {
    // Given: this fork's checkout `origin` is a local path and its registry
    // `origin` is acme/demo, so its own repository is acme/demo; `child`, a
    // fork of this fork (upstream acme/demo, own repository acme/child);
    // the forge answers.
    let fixture = fixture_with("", ORIGIN);
    register(&fixture, "child", "acme/demo", "acme/child");

    // When: #13 is named by the child's registry key.
    let output = on(&fixture, "child#13");

    // Then: the registry's `origin` is this fork's own repository, so the
    // child's upstream is this fork's: its review pull request (N119).
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);
}

#[test]
fn a_fork_of_this_fork_is_this_forks_beside_a_local_registry_origin() {
    // Given: this fork's checkout `origin` is acme/demo and its registry
    // `origin` a local path, so its own repository is acme/demo; `child`, a
    // fork of this fork (upstream acme/demo, own repository acme/child);
    // the forge answers.
    let fixture = fixture_with(ORIGIN, "");
    register(&fixture, "child", "acme/demo", "acme/child");

    // When: #13 is named by the child's registry key.
    let output = on(&fixture, "child#13");

    // Then: the checkout's `origin` is this fork's own repository, so the
    // child's upstream is this fork's: its review pull request, whatever
    // the registry's `origin` names (N125, N119's mirror).
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);
}

#[test]
fn offline_the_forks_new_name_with_no_fork_of_it_registered_is_refused_as_before() {
    // Given: config A, no fork of this fork registered, and the forge down.
    let fixture = fixture_with(CONFIG_A.0, CONFIG_A.1);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: #13 is named by acme/demo, which nothing here spells.
    let output = on(&fixture, "acme/demo#13");

    // Then: as before N83's check: whether this fork's repository was
    // renamed to acme/demo could not be asked, so nothing is recorded.
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(
            "nothing recorded: acme/demo in acme/demo#13 is no repository the registry or this \
             checkout names"
        ),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_deleted_dependents_command_takes_no_review_pull_request_of_another_number() {
    // Given: feat/ghost, never a branch here, requires the fork's #14, whose
    // head landed, and #13 under the former name, which the forge cannot
    // look up.
    let fixture = fixture();
    fixture.hold(
        "feat/ghost",
        &["origin:acme/demo#14", "origin:acme-old/demo#13"],
    );
    fixture.forge_now(ForgeNow::FormerNameFails);

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the command names #13 alone (N75), and takes it back.
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/ghost still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/ghost: {report}"));
    assert!(
        problem.ends_with("`knives depends feat/ghost --forget origin:acme-old/demo#13`"),
        "{problem}"
    );
    assert!(!problem.contains("too,"), "{problem}");
    let _ = fixture.depends_of("feat/ghost", &["--forget", "origin:acme-old/demo#13"]);
    assert_eq!(fixture.required("feat/ghost"), ["origin:acme/demo#14"]);
}

#[test]
fn a_deleted_dependents_command_leaves_a_met_review_pull_request_of_its_number() {
    // Given: feat/ghost, never a branch here, requires the fork's #14, whose
    // head landed, and #14 on another fork's repository and on a live
    // repository that is no fork's.
    let fixture = fixture();
    fixture.hold(
        "feat/ghost",
        &[
            "origin:acme/demo#14",
            "origin:acme/other#14",
            "origin:acme/sibling#14",
        ],
    );

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the command names the two records on other repositories alone
    // (N68), and takes back those two.
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/ghost still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/ghost: {report}"));
    assert!(
        problem.ends_with(
            "`knives depends feat/ghost --forget origin:acme/other#14 --forget \
             origin:acme/sibling#14`"
        ),
        "{problem}"
    );
    let _ = fixture.depends_of(
        "feat/ghost",
        &[
            "--forget",
            "origin:acme/other#14",
            "--forget",
            "origin:acme/sibling#14",
        ],
    );
    assert_eq!(fixture.required("feat/ghost"), ["origin:acme/demo#14"]);
}

#[test]
fn a_deleted_dependents_command_for_an_unplaced_member_runs_in_one_step() {
    // Given: feat/ghost, never a branch here, requires this fork's
    // feat/landed, which has landed, and feat/landed on a repository the
    // forge cannot be asked about; the forge is down.
    let fixture = fixture();
    let unplaced = "origin:gone/demo:feat/landed";
    fixture.hold("feat/ghost", &["origin:acme/demo:feat/landed", unplaced]);
    install_failing_gh(fixture.shim.path(), &fixture.log);

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the problem names the unplaced record; its command names both
    // records, says it takes the landed one too, and takes both back with
    // the forge still down (Sol's R10-N3, N72).
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/ghost still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/ghost: {report}"));
    assert!(
        problem.starts_with(&format!(
            "cannot tell whether feat/ghost still needs {unplaced}: "
        )),
        "{problem}"
    );
    assert!(
        problem.contains(
            "the command takes back origin:acme/demo:feat/landed too, which may be the same \
             requirement under another name of its repository"
        ),
        "{problem}"
    );
    let command = format!(
        "`knives depends feat/ghost --forget origin:acme/demo:feat/landed --forget {unplaced}`"
    );
    assert!(problem.ends_with(&command), "{problem}");
    let output = fixture.knives(&[
        "--text",
        "depends",
        "feat/ghost",
        "--forget",
        "origin:acme/demo:feat/landed",
        "--forget",
        unplaced,
    ]);
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(fixture.required("feat/ghost"), Vec::<String>::new());
}

#[test]
fn without_the_forge_a_member_record_on_another_repository_cannot_be_judged() {
    // Given: feat/alpha requires feat/landed recorded under a repository no
    // spelling here gives.
    let fixture = fixture();
    fixture.hold("feat/alpha", &["origin:gone/demo:feat/landed"]);

    // When: status runs without the forge.
    let output = fixture.knives(&["--json", "status", "demo", "--no-github"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("status JSON ({error}): {}", said(&output)));

    // Then: it is not read as this fork's landed member (N75).
    assert!(
        problems(&report).contains(
            &"cannot check whether feat/alpha still needs origin:gone/demo:feat/landed: whether \
              gone/demo is demo's own repository could not be asked: no forge consulted"
                .to_owned()
        ),
        "{report}"
    );
}

#[test]
fn naming_a_member_beside_another_forks_branch_of_its_name_records_this_forks_member() {
    // Given: feat/alpha holds another fork's branch named feat/landed, as an
    // older knives or a hand edit could leave.
    let fixture = fixture();
    fixture.hold("feat/alpha", &["origin:acme/sibling:feat/landed"]);

    // When: this fork's feat/landed is named.
    let output = fixture.depends(&["--on", "feat/landed"]);

    // Then: it is recorded as this fork's member, beside the other fork's
    // branch (N61(b)).
    let mut held = fixture.required("feat/alpha");
    held.sort();
    assert_eq!(
        held,
        [
            "origin:acme/demo:feat/landed",
            "origin:acme/sibling:feat/landed"
        ],
        "{output}"
    );
    // And: no line says the two may be one requirement (N85).
    assert!(!output.contains("is recorded beside"), "{output}");
}

#[test]
fn without_the_forge_a_member_not_shown_landed_is_not_known_to_have_landed() {
    // Given: feat/alpha requires feat/base, which has not landed.
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);

    // When: status runs without the forge, with the landed probe.
    let output = fixture.knives(&["--json", "status", "demo", "--no-github"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("status JSON ({error}): {}", said(&output)));

    // Then: an unmet finding that does not say it has not landed: a squash
    // merge, which only the forge shows, could have landed it (N63).
    assert!(
        details(&report, "unmet-dependency").contains(
            &"feat/alpha requires origin:acme/demo:feat/base, which is not known to have \
              landed: the forge was not asked (--no-github), so a squash merge cannot be seen"
                .to_owned()
        ),
        "{report}"
    );
    assert_eq!(output.status.code(), Some(1), "{report}");
}

#[test]
fn a_deleted_dependents_command_takes_its_review_pull_request_back_in_one_step() {
    // Given: feat/ghost, never a branch here, requires the fork's #14 under
    // both names; the former name's lookup fails, so that record cannot be
    // judged while the other is met.
    let fixture = fixture();
    fixture.hold(
        "feat/ghost",
        &["origin:acme/demo#14", "origin:acme-old/demo#14"],
    );
    fixture.forge_now(ForgeNow::FormerNameFails);

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the command names every record of #14 (N67), and taking it back
    // needs no second step, whatever the forge says.
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/ghost still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/ghost: {report}"));
    let command = "`knives depends feat/ghost --forget origin:acme-old/demo#14 --forget \
                   origin:acme/demo#14`";
    assert!(problem.ends_with(command), "{problem}");
    let _ = fixture.depends_of(
        "feat/ghost",
        &[
            "--forget",
            "origin:acme-old/demo#14",
            "--forget",
            "origin:acme/demo#14",
        ],
    );
    assert_eq!(fixture.required("feat/ghost"), Vec::<String>::new());
}

#[test]
fn a_deleted_dependents_command_takes_another_forks_branch_alone() {
    // Given: feat/ghost, never a branch here, requires this fork's
    // feat/landed, which has landed, and another fork's branch of that name.
    let fixture = fixture();
    let sibling = "origin:acme/sibling:feat/landed";
    fixture.hold("feat/ghost", &["origin:acme/demo:feat/landed", sibling]);

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the problem names the other fork's branch alone, and its command
    // spells that record, which leaves this fork's member (Sol's R9-N1).
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/ghost still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/ghost: {report}"));
    assert!(
        problem.ends_with(&format!("`knives depends feat/ghost --forget {sibling}`")),
        "{problem}"
    );
    let _ = fixture.depends_of("feat/ghost", &["--forget", sibling]);
    assert_eq!(
        fixture.required("feat/ghost"),
        ["origin:acme/demo:feat/landed"]
    );
}

#[test]
fn a_deleted_dependents_requirement_on_a_fork_with_a_local_upstream_is_in_its_one_problem() {
    // Given: a fork whose upstream is a filesystem path; feat/ghost, never a
    // branch here, requires its #3.
    let fixture = fixture();
    register_local(&fixture);
    fixture.hold("feat/ghost", &["local#3"]);

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the requirement is in feat/ghost's one problem, with its forget
    // command, not a problem of its own (N64).
    let problems = problems(&report);
    assert!(
        problems.iter().any(|problem| problem
            .starts_with("cannot tell whether feat/ghost still needs local#3: ")
            && problem.ends_with("`knives depends feat/ghost --forget local#3`")),
        "{problems:?}"
    );
    assert!(
        !problems
            .iter()
            .any(|problem| problem.starts_with("local: upstream ")),
        "{problems:?}"
    );
}

#[test]
fn a_forget_the_forge_could_not_decide_names_only_what_it_might_also_take() {
    // Given: feat/alpha requires the fork's #13 under both of its names.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    fixture.hold("feat/alpha", &[F13_OLD, F13]);

    // When: the new name's record is forgotten with the forge down.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", F13]);

    // Then: the refusal names only the record the text might also take,
    // gives the forge's one error once, and says how to take back both.
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "nothing recorded: whether {F13} also names {F13_OLD} could not be asked of the \
             forge: "
        )),
        "{stderr}"
    );
    assert_eq!(stderr.matches("gh repo view").count(), 1, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "`knives depends feat/alpha --forget {F13_OLD} --forget {F13}`"
        )),
        "{stderr}"
    );
    assert_eq!(fixture.required("feat/alpha"), [F13_OLD, F13]);
}

#[test]
fn a_deleted_dependents_command_names_a_numbered_member_by_its_record() {
    // Given: feat/ghost, never a branch here, requires the upstream's
    // merged #7 and a member branch legally named `maintainer/demo#7`,
    // which this checkout does not hold.
    let fixture = fixture();
    let member = "origin:acme/demo:maintainer/demo#7";
    fixture.hold("feat/ghost", &["maintainer/demo#7", member]);

    // When: status reports the fork.
    let (_, report) = status(&fixture);

    // Then: the one problem names the member, and its command spells the
    // member's record: its branch name alone would also name #7.
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/ghost still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/ghost: {report}"));
    assert!(
        problem.ends_with(&format!("`knives depends feat/ghost --forget {member}`")),
        "{problem}"
    );

    // And: that command takes back the member alone.
    let _ = fixture.depends_of("feat/ghost", &["--forget", member]);
    assert_eq!(fixture.required("feat/ghost"), ["maintainer/demo#7"]);
}

#[test]
fn a_deleted_dependent_not_shown_landed_without_the_probe_says_so() {
    // Given: feat/done requires feat/base; the trunk has feat/done's work;
    // then feat/done is deleted.
    let fixture = fixture();
    let lab = &fixture.lab;
    lab.branch("feat/done", "done.txt", "done\n");
    lab.upstream_trunk_file("done.txt", "done\n");
    fixture.depends_of("feat/done", &["--on", "feat/base"]);
    let done = lab::commit_at(lab, "feat/done");
    lab.jj_work(["bookmark", "delete", "feat/done"]);

    // When: status reports the fork without the landed probe.
    let output = fixture.knives(&["--json", "status", "demo", "--no-landed"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("status JSON ({error}): {}", said(&output)));

    // Then: its requirements are checked, and the note says its commit could
    // not be shown landed, not that it has not landed.
    let notes: Vec<&str> = report["notes"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        notes.contains(
            &format!(
                "feat/done is gone here and on origin; its requirements are checked because the \
                 newest commit the ledger records for it, {}, could not be shown landed: the \
                 landed probe did not run",
                done.short()
            )
            .as_str()
        ),
        "{notes:?}"
    );
}

#[test]
fn a_deleted_dependents_command_takes_back_its_open_member_by_any_name() {
    // Given: feat/done requires feat/landed, which has landed, and feat/base,
    // which has not, under both names of the fork's repository; the trunk
    // has feat/done's own work; then feat/done is deleted.
    let fixture = fixture_with(CONFIG_B.0, CONFIG_B.1);
    let lab = &fixture.lab;
    lab.branch("feat/done", "done.txt", "done\n");
    lab.upstream_trunk_file("done.txt", "done\n");
    fixture.hold("feat/done", &[M_OLD, M, "origin:acme/demo:feat/landed"]);
    lab.jj_work(["bookmark", "delete", "feat/done"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: one problem, of the open member alone, whose command names each
    // record of it as spelled: the former name's record, which the forge
    // placed, too, and the problem says so.
    let problem = problems(&report)
        .into_iter()
        .find(|problem| problem.starts_with("cannot tell whether feat/done still needs "))
        .unwrap_or_else(|| panic!("no problem of feat/done: {report}"));
    assert!(!problem.contains("feat/landed"), "{problem}");
    assert!(
        problem.contains(&format!("the command takes back {M_OLD} too")),
        "{problem}"
    );
    assert!(
        problem.ends_with(&format!(
            "`knives depends feat/done --forget {M_OLD} --forget {M}`"
        )),
        "{problem}"
    );
    assert_eq!(code, Some(3), "{report}");

    // And: that command, run while the forge is down, clears the problem.
    install_failing_gh(fixture.shim.path(), &fixture.log);
    let _ = fixture.depends_of("feat/done", &["--forget", M_OLD, "--forget", M]);
    fixture.forge_now(ForgeNow::Up);
    let (code, report) = status(&fixture);
    assert!(
        !problems(&report)
            .iter()
            .any(|problem| problem.contains("feat/done")),
        "{report}"
    );
    assert_eq!(
        fixture.required("feat/done"),
        ["origin:acme/demo:feat/landed"]
    );
    assert_ne!(code, Some(3), "{report}");
}

/// Where a dependent is held when `status` checks its requirements.
#[derive(Clone, Copy, Debug)]
enum Holder {
    /// A branch row here.
    Row,
    /// Only as `<branch>@origin`, another machine's.
    OnOrigin,
    /// Deleted; the commit the ledger records for it is here, not landed.
    DeletedOpen,
    /// Deleted; the commit the ledger records for it has landed.
    DeletedLanded,
    /// Never a branch here; the ledger records no commit for it.
    NeverRecorded,
}

/// Each holder with its dependents' prefix and its own upstream pull
/// requests, merged and open: no two holders require one number, so a
/// row's requirement never gets another holder's asked of the forge.
const HOLDERS: [(Holder, &str, (u64, u64)); 5] = [
    (Holder::Row, "row", (31, 41)),
    (Holder::OnOrigin, "orig", (32, 42)),
    (Holder::DeletedOpen, "dopen", (33, 43)),
    (Holder::DeletedLanded, "dland", (34, 44)),
    (Holder::NeverRecorded, "never", (35, 45)),
];

impl Holder {
    /// Whether `status` checks its requirements as a row's: a row, a copy on
    /// origin, or a recorded commit here that has not landed. Otherwise its
    /// later work cannot be read, and what is not met is one problem.
    const fn checked(self) -> bool {
        matches!(self, Self::Row | Self::OnOrigin | Self::DeletedOpen)
    }
}

/// The requirements of the status table, each with its short name: this
/// fork's upstream pull request, merged or open (each holder's own number,
/// [`HOLDERS`]); its review pull request whose head landed (#14) or did not
/// (#13), and #14 recorded under the repository's former name; a member
/// branch that landed or did not; another fork's upstream pull request it
/// does not have, has merged, or has open; a review pull request on a
/// repository that is not this fork's own; a branch of another fork's own
/// repository named as a landed member of this one; an upstream pull
/// request of a fork the registry does not keep; a landed member's name on
/// a repository the forge does not resolve, on a live repository that is
/// not this fork's and no registry fork's, and on a former name nothing here
/// spells, which the forge resolves to the fork's own repository.
const KINDS: [(&str, &str); 16] = [
    ("um", "maintainer/demo#<merged>"),
    ("uo", "maintainer/demo#<open>"),
    ("fl", "origin:acme/demo#14"),
    ("fr", "origin:acme-old/demo#14"),
    ("fo", "origin:acme/demo#13"),
    ("ml", "origin:acme/demo:feat/landed"),
    ("mo", "origin:acme/demo:feat/base"),
    ("so", "maintainer/sibling#5"),
    ("sm", "maintainer/sibling#51"),
    ("sx", "maintainer/sibling#52"),
    ("or", "origin:acme/other#12"),
    ("xm", "origin:acme/sibling:feat/landed"),
    ("nr", "nobody/else#3"),
    ("ym", "origin:gone/demo:feat/landed"),
    ("zm", "origin:acme-older/demo:feat/landed"),
    ("om", "origin:acme/other:feat/landed"),
];

/// `requirement`, a [`KINDS`] entry, with a holder's own `numbers`.
fn numbered_for(requirement: &str, (merged, open): (u64, u64)) -> String {
    requirement
        .replace("<merged>", &merged.to_string())
        .replace("<open>", &open.to_string())
}

/// What `status` says of one dependent's one requirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Said {
    /// Nothing: met.
    Met,
    /// An unmet-dependency finding.
    Unmet,
    /// A problem: it could not be judged.
    Unknown,
    /// Its deleted dependent's one problem, `cannot tell whether …`.
    Deferred,
}

/// The status table: per forge state and requirement, what is said of a
/// dependent whose requirements are checked as a row's, and of one whose
/// later work cannot be read ([`Holder::checked`]).
fn status_table(now: ForgeNow, kind: &str) -> (Said, Said) {
    use ForgeNow::{Down, FormerNameFails, FormerNameUnseen, OwnUnseen, UpstreamUnseen};
    use Said::{Deferred, Met, Unknown, Unmet};
    // Whether the forge answers what judging `kind` needs: this fork's
    // upstream pull requests and a member's squash evidence come from the
    // status snapshot, which needs the upstream; a review pull request needs
    // the own repository and the record's name, and, when its head did not
    // land by a replay, the snapshot too; another fork's upstream pull
    // request needs that upstream; a member that landed by a replay needs
    // nothing. The rest are never judged.
    let answered = match kind {
        "um" | "uo" | "mo" => !matches!(now, Down | UpstreamUnseen),
        "fl" => !matches!(now, Down | OwnUnseen),
        "fo" => !matches!(now, Down | OwnUnseen | UpstreamUnseen),
        "fr" => !matches!(now, Down | OwnUnseen | FormerNameFails | FormerNameUnseen),
        "sm" | "sx" => !matches!(now, Down),
        "ml" => true,
        // A member on a former name nothing here spells: the forge must name
        // it as the fork's own repository.
        "zm" => !matches!(now, Down | OwnUnseen),
        _ => false,
    };
    match (
        answered,
        matches!(kind, "um" | "fl" | "fr" | "ml" | "sm" | "zm"),
    ) {
        (false, _) => (Unknown, Deferred),
        (true, true) => (Met, Met),
        (true, false) => (Unmet, Deferred),
    }
}

/// What `report` says of `dependent`'s one requirement.
fn said_of(report: &Value, dependent: &str) -> Said {
    let problems = problems(report);
    if problems.iter().any(|problem| {
        problem.starts_with(&format!("cannot tell whether {dependent} still needs "))
    }) {
        return Said::Deferred;
    }
    if problems.iter().any(|problem| {
        problem.starts_with(&format!("{dependent} requires "))
            || problem.contains(&format!("whether {dependent} still needs "))
    }) {
        return Said::Unknown;
    }
    if details(report, "unmet-dependency")
        .iter()
        .any(|detail| detail.starts_with(&format!("{dependent} requires ")))
    {
        return Said::Unmet;
    }
    Said::Met
}

#[test]
fn status_judges_every_requirement_kind_for_every_holder_as_the_table_says() {
    // Given: one dependent per holder and requirement kind, each requiring
    // that one requirement alone; the upstream has each holder's merged and
    // open pull request.
    let fixture = fixture();
    let lab = &fixture.lab;
    fixture.forge(
        &HOLDERS
            .iter()
            .flat_map(|(_, prefix, (merged, open))| {
                [
                    pull_record(*merged, "MERGED", &format!("{prefix}-merged"), None),
                    pull_record(*open, "OPEN", &format!("{prefix}-open"), None),
                ]
            })
            .collect::<Vec<_>>(),
    );
    for (holder, prefix, numbers) in HOLDERS {
        for (kind, requirement) in KINDS {
            let name = format!("{prefix}-{kind}");
            let requirement = numbered_for(requirement, numbers);
            match holder {
                Holder::Row | Holder::DeletedOpen => {
                    lab.branch(&name, &format!("{name}.txt"), &format!("{name}\n"));
                }
                Holder::DeletedLanded => {
                    lab.branch(&name, &format!("{name}.txt"), &format!("{name}\n"));
                    lab.upstream_trunk_file(&format!("{name}.txt"), &format!("{name}\n"));
                }
                Holder::OnOrigin => {
                    lab.foreign_origin_branch("main@origin", &name, &format!("{name}\n"));
                    lab.jj_work(["git", "fetch", "--remote", "origin"]);
                }
                Holder::NeverRecorded => {}
            }
            fixture.hold(&name, &[requirement.as_str()]);
            if matches!(holder, Holder::DeletedOpen | Holder::DeletedLanded) {
                lab.jj_work(["bookmark", "delete", name.as_str()]);
            }
        }
    }

    // When / Then: status, with the forge in each state, says of each
    // dependent what the table says.
    let mut failed = Vec::new();
    for now in [
        ForgeNow::Up,
        ForgeNow::Down,
        ForgeNow::OwnUnseen,
        ForgeNow::UpstreamUnseen,
        ForgeNow::FormerNameFails,
        ForgeNow::FormerNameUnseen,
    ] {
        fixture.forge_now(now);
        let (_, report) = status(&fixture);
        let before = failed.len();
        for (holder, prefix, numbers) in HOLDERS {
            for (kind, requirement) in KINDS {
                let name = format!("{prefix}-{kind}");
                let requirement = numbered_for(requirement, numbers);
                let (checked, uncertain) = status_table(now, kind);
                let wanted = if holder.checked() { checked } else { uncertain };
                let got = said_of(&report, &name);
                if got != wanted {
                    failed.push(format!(
                        "{now:?}, {holder:?} requiring {requirement}: {got:?}; the table says \
                         {wanted:?}"
                    ));
                }
            }
        }
        if failed.len() > before {
            failed.push(format!(
                "  problems with the forge {now:?}: {:#?}\n  unmet: {:#?}",
                problems(&report),
                details(&report, "unmet-dependency")
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
