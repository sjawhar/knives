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
#[path = "common/pulls.rs"]
mod pulls;

use forge_shim::{
    install_failing_gh, install_snapshot_gh, install_snapshot_gh_with_timeline, path_with_gh_shim,
    pull_record, pull_record_with_fields,
};

use knives::commands::{
    repos,
    status::{self, OriginRelation},
    sync,
};
use knives::config::{Registry, RepoEntry};
use knives::detect::landed::RebaseOutcome;
use knives::forge::{
    ChecksSummary, Forge, ForgeError, PullFacts, PullRequest, PullSummary, RepoIdentity,
    SweepEntry, SweepPage, TimelineEvent,
};
use knives::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, WorkspaceName};
use knives::jj::{
    Repo, changed_files, changed_files_between, probe_landed, pull_heads, remote_refs,
};
use knives::store::{OwnerKind, Store};
use serde_json::Value;
use lab::Lab;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// A registry entry for the lab's work checkout, which stands in for origin.
fn lab_entry(lab: &lab::Lab) -> RepoEntry {
    RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    }
}

#[test]
fn one_batch_answers_review_age_and_checks_for_every_branch_at_once() {
    // Given: two branches with open pull requests, one stale-reviewed and red, one
    // clean, and a third whose pull request is closed
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state, decision) in [
        (11, "feat/alpha", "OPEN", "CHANGES_REQUESTED"),
        (12, "feat/beta", "OPEN", "APPROVED"),
        (13, "feat/gamma", "CLOSED", ""),
    ] {
        assert!(
            pull_requests
                .insert(
                    BranchName::new(branch),
                    PullRequest {
                        review_decision: decision.to_owned(),
                        ..pulls::pull_request(number, state, branch)
                    },
                )
                .is_none()
        );
    }
    let forge = knives::forge::fake::FakeForge {
        pull_requests,
        stale_reviews: vec![11],
        checks: BTreeMap::from([
            (
                11,
                ChecksSummary {
                    runs: vec![knives::forge::CheckRun {
                        name: "build".to_owned(),
                        conclusion: Some("FAILURE".to_owned()),
                    }],
                },
            ),
            // Supplied and empty: consulted, with nothing having run. The fake
            // answers with the facts it was given, so supplying the entry is how
            // "consulted" is expressed and omitting it is how "not consulted" is.
            (12, ChecksSummary::default()),
        ]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    // When: status gathers
    let report = status::gather(
        &knives::ids::RepoName::new("demo"),
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: each branch carries the facts the per-pull-request calls used to fetch
    let row = |name: &str| {
        report
            .branches
            .iter()
            .find(|row| row.name.as_str() == name)
            .unwrap_or_else(|| panic!("no row for {name}: {report:?}"))
            .clone()
    };
    assert_eq!(row("feat/alpha").review_stale, Some(true));
    assert!(
        row("feat/alpha")
            .checks
            .as_ref()
            .is_some_and(ChecksSummary::failing)
    );
    assert_eq!(row("feat/beta").review_stale, Some(false));
    assert_eq!(
        row("feat/beta").checks,
        Some(ChecksSummary::default()),
        "consulted with nothing running is not the same as unconsulted"
    );
    // And: a settled pull request is neither asked about nor reported on
    assert_eq!(row("feat/gamma").review_stale, None);
    assert_eq!(row("feat/gamma").checks, None);
    assert!(report.problems.is_empty(), "was: {report:?}");
}

#[test]
fn a_measured_gather_reports_the_same_report_and_a_total_that_covers_its_phases() {
    // Given: a fork with branches to probe and releases to scan
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-15", "feat/alpha", "feat/beta");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let options = || knives::commands::status::Options {
        probe: true,
        forge: None,
        cache: None,
        registry: None,
        ledger: None,
        workers: 1,
    };

    // When: the same repository is gathered with and without measurement
    let plain = status::gather(&name, &entry, &store, &options()).expect("gather");
    let (measured, timings) =
        status::gather_timed(&name, &entry, &store, &options()).expect("gather_timed");

    // Then: the report is the same one, and the total covers the phases it timed
    assert_eq!(
        status::render::render(&plain, true),
        status::render::render(&measured, true),
        "measuring changed the report"
    );
    assert!(
        timings.total >= timings.releases + timings.probes,
        "total {:?} does not cover releases {:?} plus probes {:?}",
        timings.total,
        timings.releases,
        timings.probes
    );
    assert!(
        timings.probes > std::time::Duration::ZERO,
        "two branches were probed and the probe phase measured nothing"
    );
}

/// Records what the batch was asked for, so "once, with exactly these numbers"
/// is asserted rather than assumed.
struct CountingForge {
    pull_requests: BTreeMap<BranchName, PullRequest>,
    asked: std::sync::Mutex<Vec<Vec<u64>>>,
}

impl Forge for CountingForge {
    fn repo_identity(&self, _repo: &std::path::Path) -> Result<RepoIdentity, ForgeError> {
        Ok(RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        })
    }

    fn list_pull_requests(
        &self,
        _repo: &std::path::Path,
        _authors: &[String],
    ) -> Result<Vec<PullSummary>, ForgeError> {
        Ok(self.pull_requests.values().map(PullSummary::of).collect())
    }

    fn sweep(
        &self,
        _repo: &std::path::Path,
        _target: &RepoIdentity,
    ) -> Result<SweepPage, ForgeError> {
        let mut entries = self
            .pull_requests
            .values()
            .map(|pull| SweepEntry {
                number: pull.number,
                updated_at: pull.updated_at.clone(),
                state: pull.state.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.number.cmp(&right.number))
        });
        Ok(SweepPage {
            entries,
            has_next_page: false,
        })
    }

    fn pull_facts(
        &self,
        _repo: &std::path::Path,
        _target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
        self.asked.lock().expect("lock").push(numbers.to_vec());
        Ok(numbers
            .iter()
            .filter_map(|number| {
                self.pull_requests
                    .values()
                    .find(|pull| pull.number == *number)
                    .map(|pull| {
                        (
                            *number,
                            PullFacts {
                                pull: pull.clone(),
                                details: knives::forge::PullDetails {
                                    review_predates_head: Some(false),
                                    checks: None,
                                    diff: None,
                                    head_ref_deleted: None,
                                    tip_commit_empty: None,
                                },
                                newest_comment: None,
                            },
                        )
                    })
            })
            .collect())
    }

    fn pull_timeline(
        &self,
        _repo: &std::path::Path,
        _target: &RepoIdentity,
        _number: u64,
    ) -> Result<Vec<TimelineEvent>, ForgeError> {
        Ok(Vec::new())
    }
}

#[test]
fn the_forge_is_asked_once_for_the_whole_report_with_one_entry_per_number() {
    // The point of the batch, stated as a contract rather than as a hope: one
    // call for the repository, carrying each number once, and only the numbers
    // the per-branch calls would have asked about.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.branch("feat/delta", "delta.txt", "delta\n");
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state, decision) in [
        (12, "feat/beta", "OPEN", "APPROVED"),
        (11, "feat/alpha", "OPEN", ""),
        // Every report-surfaced pull is now in the live facts batch. The settled
        // row remains intentionally absent from review/check rendering.
        (13, "feat/gamma", "CLOSED", ""),
    ] {
        assert!(
            pull_requests
                .insert(
                    BranchName::new(branch),
                    PullRequest {
                        review_decision: decision.to_owned(),
                        ..pulls::pull_request(number, state, branch)
                    },
                )
                .is_none()
        );
    }
    let forge = CountingForge {
        pull_requests,
        asked: std::sync::Mutex::new(Vec::new()),
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    let report = status::gather(
        &knives::ids::RepoName::new("demo"),
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");
    assert_eq!(report.branches.len(), 4, "was: {report:?}");
    let gamma = report
        .branches
        .iter()
        .find(|row| row.name.as_str() == "feat/gamma")
        .expect("closed pull request row");
    assert_eq!(
        gamma.checks, None,
        "settled pulls do not render a checks cell"
    );

    let asked = forge.asked.lock().expect("lock");
    assert_eq!(
        asked.len(),
        1,
        "the forge was asked {} times: {asked:?}",
        asked.len()
    );
    assert_eq!(
        asked[0],
        vec![11, 12, 13],
        "every report-surfaced pull is fetched once, in sorted order"
    );
    drop(asked);
}

#[test]
fn a_failed_facts_batch_clears_review_and_check_cells() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let mut pull = sync_pull_request(11, "OPEN", "feat/alpha", "head-11");
    pull.review_decision = "APPROVED".to_owned();
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(BranchName::new("feat/alpha"), pull)]),
        stale_reviews: vec![11],
        checks: BTreeMap::from([(
            11,
            ChecksSummary {
                runs: vec![knives::forge::CheckRun {
                    name: "build".to_owned(),
                    conclusion: Some("FAILURE".to_owned()),
                }],
            },
        )]),
        fail_facts: true,
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    let report = status::gather(
        &knives::ids::RepoName::new("demo"),
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    assert!(!report.forge_consulted, "was: {report:?}");
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("pull request state unavailable")),
        "was: {report:?}"
    );
    assert_eq!(status::exit_for(&report), knives::cli::Exit::Incomplete);
    let row = &report.branches[0];
    assert_eq!(row.pull_request, None, "no live-looking pull request cell");
    assert_eq!(row.review_stale, None, "a refused answer is not current");
    assert_eq!(row.checks, None, "a refused answer is not no checks");
}

#[test]
fn a_consulted_false_report_carries_zero_pull_facts() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let target = knives::ids::BranchTarget::new(name.clone(), BranchName::new("feat/alpha"));
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(11, "OPEN", "feat/alpha", "head-11"),
        )]),
        fail_facts: true,
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.track_pull(&target, 42);

    let report = status::gather(
        &name,
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    assert!(!report.forge_consulted, "was: {report:?}");
    assert!(
        report.branches.iter().all(|row| row.pull_request.is_none()),
        "a failed facts batch leaked a pull fact: {report:?}"
    );
    assert_eq!(
        report.branches[0]
            .stated_pull
            .as_ref()
            .map(|pull| (pull.number, pull.state.as_str())),
        Some((42, "unknown")),
        "the stated pull escaped the failed batch: {report:?}"
    );
    assert!(!report.problems.is_empty(), "was: {report:?}");
}

#[test]
fn stated_pulls_and_dependencies_are_answered_from_the_one_batch() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let target = knives::ids::BranchTarget::new(name.clone(), BranchName::new("feat/alpha"));
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(11, "OPEN", "feat/alpha", "head-11"),
        )]),
        vanished_states: BTreeMap::from([(42, "CLOSED".to_owned()), (43, "MERGED".to_owned())]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.track_pull(&target, 42);
    store.add_dependencies(
        &target,
        &[knives::ids::Requirement {
            repo: name.clone(),
            number: 43,
        }],
    );
    let registry = Registry {
        repos: BTreeMap::from([("demo".to_owned(), lab_entry(&lab))]),
        ..Registry::default()
    };

    let report = status::gather(
        &name,
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: Some(&registry),
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    assert_eq!(
        report.branches[0]
            .stated_pull
            .as_ref()
            .map(|pull| (pull.number, pull.state.as_str())),
        Some((42, "CLOSED")),
        "the stated number did not come from the snapshot: {report:?}"
    );
    assert!(
        report.findings.is_empty(),
        "a merged dependency became unmet: {report:?}"
    );
    assert!(report.problems.is_empty(), "was: {report:?}");
    assert!(
        status::render::render(&report, true).contains("#42 closed (stated)"),
        "the stated batch answer did not render: {report:?}"
    );
}

#[test]
fn landed_verdicts_come_from_the_cache_when_the_key_matches() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let forge = knives::forge::fake::FakeForge::default();
    let state = tempfile::tempdir().expect("state directory");
    let cache = tempfile::tempdir().expect("cache directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let options = || knives::commands::status::Options {
        probe: true,
        forge: Some(&forge),
        cache: Some(cache.path()),
        registry: None,
        ledger: None,
        workers: 1,
    };

    let first = status::gather(&name, &entry, &store, &options()).expect("first gather");
    assert_eq!(
        first.branches[0].landed,
        Some(knives::detect::LandedVerdict::NotInTrunk),
        "the fixture must have a fresh probe answer: {first:?}"
    );
    let identity = RepoIdentity {
        name_with_owner: "fake-owner/fake-repo".to_owned(),
        id: "FAKEID".to_owned(),
    };
    let cache_file = knives::forge_cache::cache_path(cache.path(), &identity).expect("cache path");
    let repo = Repo::open(&lab.work).expect("open repository");
    let tip = repo.resolve_commit("feat/alpha").expect("feature tip");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk tip");
    let key = knives::forge_cache::landed_key(&tip, &trunk);
    let mut persisted = knives::forge_cache::load(&cache_file, &identity).expect("cache file");
    let _ = persisted
        .landed
        .insert(key, knives::detect::LandedVerdict::InTrunk);
    knives::forge_cache::write(&cache_file, &persisted).expect("poison landed cache entry");

    let cached = status::gather(&name, &entry, &store, &options()).expect("cached gather");
    assert_eq!(
        cached.branches[0].landed,
        Some(knives::detect::LandedVerdict::InTrunk),
        "the matching cache key was not read: {cached:?}"
    );

    extend_branch(&lab, "feat/alpha", "alpha-next.txt", "next\n");
    let fresh = status::gather(&name, &entry, &store, &options()).expect("fresh gather");
    assert_eq!(
        fresh.branches[0].landed,
        Some(knives::detect::LandedVerdict::NotInTrunk),
        "a new branch tip reused an old landed cache entry: {fresh:?}"
    );
}

#[test]
fn a_probe_free_run_preserves_the_landed_section() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let forge = knives::forge::fake::FakeForge::default();
    let state = tempfile::tempdir().expect("state directory");
    let cache = tempfile::tempdir().expect("cache directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let with_probe = || knives::commands::status::Options {
        probe: true,
        forge: Some(&forge),
        cache: Some(cache.path()),
        registry: None,
        ledger: None,
        workers: 1,
    };
    let without_probe = || knives::commands::status::Options {
        probe: false,
        forge: Some(&forge),
        cache: Some(cache.path()),
        registry: None,
        ledger: None,
        workers: 1,
    };

    status::gather(&name, &entry, &store, &with_probe()).expect("probe gather");
    let identity = RepoIdentity {
        name_with_owner: "fake-owner/fake-repo".to_owned(),
        id: "FAKEID".to_owned(),
    };
    let cache_file = knives::forge_cache::cache_path(cache.path(), &identity).expect("cache path");
    let before = knives::forge_cache::load(&cache_file, &identity)
        .expect("cache after probe")
        .landed;
    assert!(!before.is_empty(), "the probe wrote no landed entries");

    status::gather(&name, &entry, &store, &without_probe()).expect("probe-free gather");
    let after = knives::forge_cache::load(&cache_file, &identity)
        .expect("cache after probe-free run")
        .landed;
    assert_eq!(
        after, before,
        "a probe-free run erased landed cache entries"
    );
}

#[test]
fn an_unresolvable_trunk_fails_loudly_and_touches_no_landed_cache() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let forge = knives::forge::fake::FakeForge::default();
    let state = tempfile::tempdir().expect("state directory");
    let cache = tempfile::tempdir().expect("cache directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let options = || knives::commands::status::Options {
        probe: true,
        forge: Some(&forge),
        cache: Some(cache.path()),
        registry: None,
        ledger: None,
        workers: 1,
    };

    status::gather(&name, &entry, &store, &options()).expect("initial probe");
    let identity = RepoIdentity {
        name_with_owner: "fake-owner/fake-repo".to_owned(),
        id: "FAKEID".to_owned(),
    };
    let cache_file = knives::forge_cache::cache_path(cache.path(), &identity).expect("cache path");
    let before = std::fs::read(&cache_file).expect("cache after initial probe");
    let mut unresolvable = entry;
    unresolvable.base = Some("missing-trunk".to_owned());

    let error = status::gather(&name, &unresolvable, &store, &options())
        .expect_err("an unresolvable configured trunk must fail loudly");
    assert!(
        error.to_string().contains("missing-trunk@upstream"),
        "missing unresolved revision in error: {error:#}"
    );
    let after = std::fs::read(&cache_file).expect("cache after unresolvable-trunk run");
    assert_eq!(
        after, before,
        "an unresolvable-trunk run rewrote landed cache"
    );
}

fn relation_to_origin(lab: &lab::Lab) -> Result<Option<OriginRelation>, knives::jj::JjError> {
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let origin_tip = repo
        .resolve_commit("feat/alpha@origin")
        .expect("origin tip");

    status::phases::relation_to_origin(&repo, &tip, Some(&origin_tip))
}

/// Registry home + consumer for release-cut tests: one repo named `demo`,
/// one consumer following the current release by branch.
fn release_test_home(lab: &lab::Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", branch = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", branch = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write registry");
    (home, consumer)
}

fn sync_entry(lab: &lab::Lab) -> RepoEntry {
    RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    }
}

fn sync_pull_request(number: u64, state: &str, branch: &str, head: &str) -> PullRequest {
    PullRequest {
        head_ref_oid: head.to_owned(),
        updated_at: "2026-08-15T00:00:00Z".to_owned(),
        ..pulls::pull_request(number, state, branch)
    }
}

#[test]
fn a_fork_whose_trunk_is_dev_probes_and_forks_against_dev() {
    // Given: an upstream whose only branch is dev, and a feature branch on it
    let lab = Lab::with_trunk("dev");
    lab.branch("feat/alpha", "feature.txt", "content\n");
    // When: the landed probe measures against dev@upstream
    let outcome = knives::jj::probe_landed(
        lab.work_path(),
        &knives::ids::BranchName::new("feat/alpha"),
        "dev@upstream",
    )
    .expect("probe runs");
    // Then: unmerged work replays clean and non-empty — the probe found the
    // trunk rather than erroring on a nonexistent main
    assert_eq!(outcome, RebaseOutcome::CleanNonEmpty);

    lab.publish_pull("feat/alpha", 1);
    lab.squash_merge_pull(1, None);
    let outcome = knives::jj::probe_landed(
        lab.work_path(),
        &knives::ids::BranchName::new("feat/alpha"),
        "dev@upstream",
    )
    .expect("probe runs after squash merge");
    assert_eq!(outcome, RebaseOutcome::Empty);
}

#[test]
fn a_jj_workspace_beside_a_registered_repo_resolves_that_repo() {
    let lab = Lab::new();
    let workspace = lab
        .work
        .parent()
        .expect("workspace parent")
        .join("feature-alpha");
    knives::jj::add_workspace(&lab.work, "feature-alpha", &workspace, "main@upstream")
        .expect("add workspace");
    let registry = Registry {
        repos: BTreeMap::from([("demo".to_owned(), sync_entry(&lab))]),
        ..Registry::default()
    };

    assert_eq!(
        registry.containing(&workspace).map(|(name, _)| name),
        Some(knives::ids::RepoName::new("demo"))
    );
    let unrelated = tempfile::tempdir().expect("unrelated directory");
    assert!(registry.containing(unrelated.path()).is_none());
}

#[test]
fn workspace_activity_attributes_working_copy_moves_to_their_workspace() {
    let lab = Lab::new();
    lab.jj_work(["workspace", "add", "--name", "feat-x", "../feat-x-ws"]);
    let workspace_dir = lab.work.parent().expect("workspace parent").join("feat-x-ws");
    std::fs::write(workspace_dir.join("w.txt"), "work\n").expect("write workspace content");
    lab.jj_at(&workspace_dir, ["new", "-m", "wip"]);

    let repo = Repo::open(&lab.work).expect("open");
    let wanted = BTreeSet::from([WorkspaceName::new("feat-x")]);
    let activity = repo.workspace_activity(&wanted, 200).expect("walk");

    assert!(
        activity.moves.contains_key(&WorkspaceName::new("feat-x")),
        "was: {activity:?}"
    );
}

#[test]
fn workspace_activity_reports_nothing_for_a_workspace_that_never_moved() {
    let lab = Lab::new();
    let repo = Repo::open(&lab.work).expect("open");
    let wanted = BTreeSet::from([WorkspaceName::new("never-created")]);
    let activity = repo
        .workspace_activity(&wanted, operation_ids(&lab.work).len())
        .expect("walk");

    assert!(activity.moves.is_empty(), "was: {activity:?}");
    assert!(activity.horizon.is_none(), "was: {activity:?}");
}

#[test]
fn cli_dispatch_records_an_observation_before_running_the_command() {
    let lab = Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"o\"\n",
            lab.work.display(),
            lab.upstream.display()
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "repos"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("run knives");

    assert!(
        output.status.success(),
        "knives failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("seen.json"))
            .expect("CLI dispatch records seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["owners"]["harness-session"]["agent-one"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
    assert!(
        seen["workspaces"]["demo/work"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
}

#[test]
fn a_forgotten_and_abandoned_release_disappears_and_the_remote_keeps_it() {
    // Given: a pushed release-shaped merge and a chained feature pair. Forget
    // alone leaves the remote-tracking ref pinning the release (abandon then
    // refuses "immutable"); forget --include-remotes releases the pin. The
    // chain requires multi-id abandon to act in one invocation.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.jj_work(["new", "-r", "feat/alpha", "-m", "feat/alpha-child"]);
    std::fs::write(lab.work.join("alpha-child.txt"), "alpha child\n").expect("write child");
    lab.jj_work(["bookmark", "set", "feat/alpha-child", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let release = repo
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let alpha_child = repo
        .resolve_commit("feat/alpha-child")
        .expect("resolve alpha child");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");

    // When: the release is reaped in the load-bearing order, then the chained
    // features in one batch.
    let outcome = knives::jj::forget_and_abandon(
        &lab.work,
        &[("release/2026-08-04".to_owned(), vec![release.clone()])],
        "knives: reap release/2026-08-04",
    )
    .expect("reap the release");
    assert!(outcome.refused.is_empty(), "release abandon refused");
    let outcome = knives::jj::forget_and_abandon(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), vec![alpha.clone()]),
            ("feat/alpha-child".to_owned(), vec![alpha_child.clone()]),
            ("feat/beta".to_owned(), vec![beta.clone()]),
        ],
        "knives: reap the feature chain",
    )
    .expect("reap the feature chain");
    assert!(outcome.refused.is_empty(), "feature abandon refused");

    // Then: no ref of any kind remains and every abandoned commit is invisible.
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips.keys().any(|r| matches!(
            r.branch().as_str(),
            "release/2026-08-04" | "feat/alpha" | "feat/alpha-child" | "feat/beta"
        )),
        "reaped refs survived: {tips:?}"
    );
    // Visibility check, verified empirically: naming a hidden commit id in a
    // revset RESURRECTS it into the resolution (`all() & <id>` still returns
    // it after abandon), so the only honest assertion is listing all() and
    // checking absence.
    let visible = knives::jj::commits_matching(&lab.work, "all()").expect("query");
    assert!(
        !visible.contains(&release)
            && !visible.contains(&alpha)
            && !visible.contains(&alpha_child)
            && !visible.contains(&beta),
        "abandoned commits still visible: {visible:?}"
    );
    let orphans =
        knives::jj::commits_matching(&lab.work, "description(glob:\"feat/alpha-child*\")")
            .expect("query orphans");
    assert!(
        orphans.is_empty(),
        "a descendant survived: one-at-a-time abandon rewrote the later ids: {orphans:?}"
    );
    assert!(
        knives::jj::commits_matching(&lab.work, "none()")
            .expect("empty revset")
            .is_empty(),
        "none() returned commits"
    );
    // And: the remote still has the branch — reaping never touches the wire.
    let on_remote = std::process::Command::new("git")
        .args([
            "ls-remote",
            "--heads",
            lab.temp_origin().to_str().expect("utf-8"),
            "release/2026-08-04",
        ])
        .output()
        .expect("ls-remote");
    assert!(
        !String::from_utf8_lossy(&on_remote.stdout).trim().is_empty(),
        "remote branch was deleted"
    );
}

#[test]
fn reap_removes_superseded_cuts_and_keeps_the_newest() {
    // Given: two dated cuts, both pushed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");

    // Then: the older cut is gone in every form, the newest survives.
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04")
    );
    assert!(
        tips.keys()
            .any(|r| r.branch().as_str() == "release/2026-08-05")
    );
}

#[test]
fn reap_refuses_a_cut_that_has_local_descendants() {
    // Given: work stacked directly on a superseded cut — #4's third loss mode.
    // Reaping must never be the thing that drops it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked work"]);
    lab.jj_work(["new"]); // park the working copy elsewhere
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");

    // Then: nothing reaped; the reason names the descendant.
    assert!(report.reaped.is_empty(), "reaped: {:?}", report.reaped);
    assert_eq!(report.kept.len(), 1);
    assert!(report.kept[0].1.contains("descendant"), "{:?}", report.kept);
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04")
    );
}

#[test]
fn release_reap_returns_findings_when_a_superseded_cut_is_kept() {
    // Given: work stacked directly on a superseded cut, which reap must preserve.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked work"]);
    lab.jj_work(["new"]);
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the real reap command sees the protected older cut.
    let output = knives_release(&lab, &home, &["reap"]);

    // Then: the actionable kept result makes the command non-zero.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo: kept release/2026-08-04"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_reap_returns_findings_when_an_untracked_remote_pin_refuses_abandon() {
    // Given: an untracked remote pin still holds a superseded dated cut immutable.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let status = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("fetch superseded cut in second clone");
    assert!(status.success(), "fetch superseded cut in second clone");
    let status = Command::new("jj")
        .args([
            "bookmark",
            "create",
            "keep/pin",
            "-r",
            "release/2026-08-04@origin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create pin in second clone");
    assert!(status.success(), "create pin in second clone");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "keep/pin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();
    let (home, _consumer) = release_test_home(&lab);

    // When: standalone reap attempts to remove the superseded cut.
    let output = knives_release(&lab, &home, &["reap"]);

    // Then: the abandon refusal is printed and exits with Findings (1).
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("demo: ! release/2026-08-04: refs forgotten, abandon refused:"),
        "{stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_second_workspaces_parked_working_copy_does_not_block_reaping() {
    // Given: another workspace parked (empty, undescribed) on the superseded
    // cut — knives' normal multi-workspace state. jj only auto-discards the
    // CURRENT workspace's @ when it moves; a second workspace's stays.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let parked_workspace = lab
        .work
        .parent()
        .expect("workspace has parent")
        .join("parked-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "parked",
        "--revision",
        "release/2026-08-04",
        parked_workspace.to_str().expect("utf-8 path"),
    ]);
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");

    // Then: the parked working copy does not block — the clause this pins.
    assert_eq!(
        report.reaped,
        vec!["release/2026-08-04".to_owned()],
        "{report:?}"
    );
}

#[test]
fn reap_reaps_when_another_remote_bookmark_pins_the_cut() {
    // Given: a non-dated origin bookmark created and pushed from work itself.
    // Fetch returns this pin as TRACKED, so it is the mutable contrast to the
    // untracked-pin sibling test.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.jj_work(["bookmark", "create", "keep/pin", "-r", "release/2026-08-04"]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "keep/pin",
    ]);
    lab.fetch_work();
    let tips = Repo::open(&lab.work)
        .expect("open fixture")
        .bookmark_tips()
        .expect("fixture tips");
    assert!(
        tips.keys().any(|reference| {
            matches!(
                reference,
                BookmarkRef::Remote { branch, remote }
                    if branch.as_str() == "keep/pin" && remote.as_str() == "origin"
            )
        }),
        "fixture expects keep/pin@origin"
    );

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");

    // Then: the tracked remote pin is mutable, so the dated cut is reaped.
    assert_eq!(
        report.reaped,
        vec!["release/2026-08-04".to_owned()],
        "{report:?}"
    );
    assert!(
        report.forgotten_only.is_empty() && report.notes.is_empty(),
        "{report:?}"
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("remaining tips");
    assert!(
        tips.keys().any(|reference| {
            matches!(
                reference,
                BookmarkRef::Remote { branch, remote }
                    if branch.as_str() == "keep/pin" && remote.as_str() == "origin"
            )
        }),
        "reaping must leave keep/pin@origin alone"
    );
}

#[test]
fn an_untracked_remote_pin_makes_abandon_refuse_and_lands_in_forgotten_only() {
    // Given: two cuts pushed; a pin bookmark on the superseded cut created in
    // ANOTHER clone and pushed from there, so it arrives in work UNTRACKED —
    // an immutable head (builtin_immutable_heads includes
    // untracked_remote_bookmarks()). The tracked-pin sibling test shows the
    // mutable contrast.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let status = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("fetch superseded cut in second clone");
    assert!(status.success(), "fetch superseded cut in second clone");
    let status = Command::new("jj")
        .args([
            "bookmark",
            "create",
            "keep/pin",
            "-r",
            "release/2026-08-04@origin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create pin in second clone");
    assert!(status.success(), "create pin in second clone");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "keep/pin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");

    // Then: refs are forgotten, abandon refuses, and reaped does not overstate.
    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(report.forgotten_only, vec!["release/2026-08-04".to_owned()]);
    assert!(
        report.notes.iter().any(|note| note.contains("immutable")),
        "{report:?}"
    );
}

#[test]
fn a_refused_first_name_does_not_stop_reaping_later_names() {
    // Given: TWO superseded dated cuts, where the alphabetically first is held
    // immutable by an untracked remote pin and the second is freely reapable.
    // The fleet cleanup of 2026-08-07 saw a reap stop at its first immutable
    // commit instead of carrying on — this pins the continuation.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-06", "feat/alpha", "feat/beta");
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    let status = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("fetch superseded cut in second clone");
    assert!(status.success(), "fetch superseded cut in second clone");
    let status = Command::new("jj")
        .args([
            "bookmark",
            "create",
            "keep/pin",
            "-r",
            "release/2026-08-04@origin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create pin in second clone");
    assert!(status.success(), "create pin in second clone");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "keep/pin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");

    // Then: the pinned first name refuses without stopping the second.
    assert_eq!(report.forgotten_only, vec!["release/2026-08-04".to_owned()]);
    assert_eq!(report.reaped, vec!["release/2026-08-05".to_owned()]);
    assert!(
        report.notes.iter().any(|note| note.contains("immutable")),
        "{report:?}"
    );
}

#[test]
fn reaping_is_one_operation_described_for_the_op_log() {
    // Given: one superseded cut. Reaping used to be two operations per name
    // (bookmark forget, then abandon), each described as raw `args: jj ...`.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    let operations_before = operation_ids(&lab.work);

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report =
        knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("reap");
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);

    // Then: the whole reap is ONE operation, described as knives' own act.
    let operations_after = operation_ids(&lab.work);
    assert_eq!(
        operations_after.len(),
        operations_before.len() + 1,
        "a reap must be one operation"
    );
    assert_eq!(
        newest_operation_description(&lab.work),
        "knives: reap release/2026-08-04"
    );
}

#[test]
fn reap_clears_a_ref_the_next_fetch_rematerialized() {
    // Given: a reaped workspace whose next fetch resurrected the superseded ref
    // as untracked (jj keeps no memory of forgotten refs; spec evidence item 2).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let repo = Repo::open(&lab.work).expect("open");
    knives::commands::release::reap_superseded(&lab.work, &repo, "origin").expect("first reap");
    lab.fetch_work();
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04"),
        "fixture expects the fetch to re-materialize the ref; it did not"
    );

    // When: reaped again (idempotence is the contract).
    let repo = Repo::open(&lab.work).expect("reopen for second reap");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo, "origin")
        .expect("second reap");

    // Then: gone again.
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);
    let tips = Repo::open(&lab.work)
        .expect("final open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04")
    );
}

#[test]
fn local_commit_after_push_is_ahead_of_origin() {
    // Given: a branch already pushed to origin and one additional local commit.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["new", "-r", "feat/alpha", "-m", "local advance"]);
    std::fs::write(lab.work.join("local.txt"), "local\n").expect("write local commit");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: the origin relation is resolved from both tips.
    let relation = relation_to_origin(&lab);

    // Then: origin is the ancestor, so local is ahead.
    assert!(relation.is_ok());
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Ahead)
    );
}

#[test]
fn origin_commit_after_push_leaves_local_branch_behind() {
    // Given: a branch pushed to origin, then advanced from another clone.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "untrack", "feat/alpha", "--remote", "origin"]);
    lab.advance_origin_branch("feat/alpha", "origin advance\n");
    lab.fetch_work();

    // When: the local and fetched origin tips are compared.
    let relation = relation_to_origin(&lab);

    // Then: origin is the descendant, so local is behind.
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Behind)
    );
}

#[test]
fn rewritten_local_branch_is_diverged_from_origin() {
    // Given: a pushed branch whose local tip was rewritten without updating origin.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.rewrite_local_branch("feat/alpha", "rewritten\n");

    // When: the origin relation is resolved from mutually unreachable tips.
    let relation = relation_to_origin(&lab);

    // Then: neither side is announced as behind the other.
    assert!(relation.is_ok());
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Diverged)
    );
}

#[test]
fn an_unresolvable_origin_tip_returns_an_error() {
    // Given: a real local branch and an origin id the repository cannot resolve.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let unresolved = CommitId::new("1111111111111111111111111111111111111111");

    // When: the resolver compares local history to that absent origin tip.
    let error = status::phases::relation_to_origin(&repo, &tip, Some(&unresolved))
        .expect_err("an unresolved origin tip must not become a relation");

    // Then: the caller receives an error to report rather than a history verdict.
    assert!(error.to_string().contains(unresolved.as_str()));
}

#[test]
fn ancestry_is_answered_in_both_directions() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    let base = repo.resolve_commit("main").expect("main");

    assert!(repo.is_ancestor(&base, &tip).expect("base is behind tip"));
    assert!(
        !repo
            .is_ancestor(&tip, &base)
            .expect("tip is not behind base")
    );
}

#[test]
fn a_fixed_pin_locked_to_an_ancestor_is_behind() {
    let lab = Lab::new();
    lab.branch("integration", "base.txt", "base\n");
    let repo = Repo::open(&lab.work).expect("open ancestor");
    let ancestor = repo.resolve_commit("integration").expect("ancestor");
    lab.jj_work(["new", "-r", "integration", "-m", "advance integration"]);
    std::fs::write(lab.work.join("advance.txt"), "advance\n").expect("advance integration");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new"]);

    let consumer = tempfile::tempdir().expect("consumer directory");
    let locked: String = ancestor.as_str().chars().take(12).collect();
    std::fs::write(
        consumer.path().join("uv.lock"),
        format!("url = \"https://forge.invalid/o/repo.git?branch=integration#{locked}\"\n"),
    )
    .expect("consumer pin");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: "https://forge.invalid/up/repo.git".to_owned(),
        origin: "https://forge.invalid/o/repo.git".to_owned(),
        base: None,
        release: None,
        release_branch: Some("integration".to_owned()),
        test_count_command: None,
        consumers: vec![consumer.path().to_owned()],
    };
    let repo = Repo::open(&lab.work).expect("open advanced branch");

    let lag = repos::pin_lag(&entry, None, Some(&repo));

    assert!(
        lag.notes
            .iter()
            .any(|note| note.contains("pins read from the working copy")),
        "notes: {:?}",
        lag.notes
    );
    assert!(
        lag.lag.as_ref().is_some_and(|lag| lag.contains(&locked)),
        "lag: {:?}",
        lag.lag
    );
}

#[test]
fn a_consumer_checkout_parked_behind_its_origin_does_not_produce_a_false_behind() {
    // Given: a consumer repo whose origin trunk pins the newest release while
    // the checkout's working copy still shows an older pin — the exact state
    // that produced false BEHIND findings twice.
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );

    // When: the consumer is scanned.
    let scan = knives::release_model::scan_consumer_for(
        &consumer,
        Some("tool"),
        &knives::ids::ReleaseScheme::Dated,
    );

    // Then: the pin is the origin trunk's, and the checkout's lag is a note.
    assert_eq!(scan.pins.len(), 1, "was: {:?}", scan.pins);
    assert_eq!(scan.pins[0].reference, "release/2026-07-28");
    assert!(
        scan.notes.iter().any(|note| note.contains("behind")),
        "the stale checkout is annotated, not silently trusted: {:?}",
        scan.notes
    );
    assert!(scan.problems.is_empty());
}

#[test]
fn a_dev_trunk_consumer_checkout_uses_its_origin_head_pin() {
    let lab = Lab::with_trunk("dev");
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );

    let scan = knives::release_model::scan_consumer_for(
        &consumer,
        Some("tool"),
        &knives::ids::ReleaseScheme::Dated,
    );

    assert_eq!(scan.pins.len(), 1, "was: {:?}", scan.pins);
    assert_eq!(scan.pins[0].reference, "release/2026-07-28");
    assert!(
        scan.notes.iter().any(|note| note.contains("origin/dev")),
        "the origin default branch is preserved: {:?}",
        scan.notes
    );
    assert!(scan.problems.is_empty());
}

#[test]
fn a_consumer_without_an_origin_remote_uses_its_current_working_copy_pin() {
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );
    lab.reset_consumer_to_origin(&consumer);
    lab.rename_consumer_remote(&consumer, "origin", "upstream");
    let entry = RepoEntry {
        path: lab.work,
        upstream: "https://forge.invalid/up/tool.git".to_owned(),
        origin: "https://forge.invalid/o/tool.git".to_owned(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: vec![consumer.clone()],
    };

    let pin_lag = repos::pin_lag(&entry, Some(&"release/2026-07-28@origin".to_owned()), None);

    assert_eq!(pin_lag.lag, None, "was: {pin_lag:?}");
    assert_eq!(
        pin_lag.notes,
        vec![format!(
            "{}: no origin trunk resolved; pins read from the working copy",
            consumer.display()
        )]
    );
}

#[test]
fn a_tip_carried_into_another_branch_is_found() {
    // Given: a maintainer branch built on our branch's tip
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    // When: bookmarks carrying the original tip are listed
    let repo = Repo::open(&lab.work).expect("reopen");
    let carriers = repo
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");
    let named: Vec<String> = carriers.iter().map(ToString::to_string).collect();

    // Then: the other branch is included and the branch itself is not
    assert!(
        named.iter().any(|name| name.contains("theirs/rework")),
        "was: {named:?}"
    );
    assert!(
        !named.iter().any(|name| name == "feat/alpha"),
        "a branch does not carry itself: {named:?}"
    );
}

#[test]
fn a_release_cut_is_not_a_carrier_locally_or_at_origin() {
    // Given: a flat release cut that carries our feature branch, then is pushed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.octopus("release/2026-07-30", "feat/alpha", "feat/beta");
    lab.push_branch("release/2026-07-30");

    // When: carriers of the feature tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");

    // Then: the release is not reported through either representation we own.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("release/2026-07-30"))),
        "local release was reported: {carriers:?}"
    );
    assert!(
        !carriers.contains(&BookmarkRef::Remote {
            branch: BranchName::new("release/2026-07-30"),
            remote: RemoteName::new("origin"),
        }),
        "origin release was reported: {carriers:?}"
    );
}

#[test]
fn a_release_cut_records_its_whole_parent_set_under_the_release_name() {
    // Given: two branches with distinct tips that the cut must preserve as evidence.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let alpha = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("alpha tip");

    // When: a first cut is taken through the binary.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-15"]);
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "cut failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the release ref is the subject, every member is named, and their
    // commit ids remain evidence for a reader to verify later.
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let cut = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-15"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert_eq!(cut.kind, knives::ledger::Kind::Event);
    assert!(cut.text.contains("feat/alpha"), "was: {}", cut.text);
    assert!(cut.text.contains("feat/beta"), "was: {}", cut.text);
    assert!(cut.text.contains("2 parent(s)"), "was: {}", cut.text);
    assert!(
        cut.evidence
            .iter()
            .any(|reference| reference == alpha.as_str()),
        "was: {:?}",
        cut.evidence
    );
    let created = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("release/2026-08-15")
        .expect("release tip");
    assert_eq!(cut.anchor.as_deref(), Some(created.as_str()));

    let second = knives_release(&lab, &home, &["cut", "release/2026-08-16"]);
    assert!(
        second.status.success() || second.status.code() == Some(1),
        "second cut failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let second_cut = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-16"))
        .unwrap_or_else(|| panic!("no second cut entry: {entries:?}"));
    assert!(
        second_cut
            .text
            .contains("previous cut release/2026-08-15 recorded 2 member(s); all carried"),
        "was: {}",
        second_cut.text
    );
}

#[test]
fn a_cut_that_omits_a_recorded_member_is_refused_until_the_drop_is_stated() {
    // Given: a three-member cut through the binary, then the release rebuilt
    // by hand without feat/gamma — the bookmark moved, so the repository no
    // longer remembers the old composition. The ledger's cut event does.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work(["new", "feat/alpha", "feat/beta", "-m", "hand-rebuilt merge"]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);

    // When: the next cut is taken without stating the drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, the missing member is named, and nothing was cut.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("refusing to cut") && stdout.contains("feat/gamma@"),
        "{stdout}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after the refusal")
            .resolve_commit("release/2026-08-05")
            .is_err(),
        "the refused cut was published anyway"
    );

    // And when: the drop is stated.
    let allowed = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);

    // Then: the cut lands and its ledger event records exactly what was dropped.
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "the stated drop was still refused: {stdout}\n{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event
            .text
            .contains("previous cut release/2026-08-04 recorded 3 member(s); dropped: feat/gamma@"),
        "was: {}",
        event.text
    );
}

#[test]
fn a_drop_between_cuts_is_restated_at_the_next_cut() {
    // Given: a member dropped through the tool itself. The drop recorded a why
    // on the release, but the next cut still ships less than the last one did,
    // and the cut is where that is stated for the record.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is taken without stating the drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut is refused and names the member the last cut recorded.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("refusing to cut") && stdout.contains("feat/beta@"),
        "{stdout}"
    );

    // And when: the drop is stated, the cut lands and records it.
    let allowed = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event.text.contains("dropped: feat/beta@"),
        "was: {}",
        event.text
    );
}

#[test]
fn a_recorded_member_that_landed_upstream_is_carried_not_dropped() {
    // Given: a member squash-merged upstream, the composition rebased onto the
    // landing, and the landed member dropped — the standard shrink that loses
    // no content, because the base now carries the member's diff.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    lab.fetch_work();
    let rebased = knives_release(&lab, &home, &["rebase", "main@upstream"]);
    assert!(rebased.status.success(), "{rebased:?}");
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "landed upstream as #7"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is taken without --allow-drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: nothing refuses — the recorded member's content is in the cut
    // through its base — and the event records a full carry.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "a landed member was treated as dropped: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event
            .text
            .contains("previous cut release/2026-08-04 recorded 2 member(s); all carried"),
        "was: {}",
        event.text
    );
}

#[test]
fn a_recorded_member_this_repository_cannot_resolve_is_named_in_the_refusal() {
    // Given: a ledger whose newest cut event names a commit this checkout has
    // never seen — a stale ledger, or a re-clone. Unverifiable must not read
    // as carried, and the gate applies even to a first cut: with every release
    // bookmark gone, the ledger is the only witness the composition existed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let ledger_dir = home.path().join("ledger").join("demo");
    std::fs::create_dir_all(&ledger_dir).expect("create ledger directory");
    std::fs::write(
        ledger_dir.join("20260801T000000.000000000Z-dead.md"),
        "+++\n\
         ts = \"2026-08-01T00:00:00Z\"\n\
         owner = \"an-agent\"\n\
         subject = \"release/2026-08-01\"\n\
         kind = \"event\"\n\
         evidence = [\"feedfeedfeedfeedfeedfeedfeedfeedfeedfeed\", \"deaddeaddeaddeaddeaddeaddeaddeaddeaddead\"]\n\
         +++\n\
         cut release/2026-08-01 as feedfeedfeed with 1 parent(s): feat/old@deaddeaddead\n",
    )
    .expect("write a cut event by hand");

    // When: a first cut is taken with no release bookmark anywhere.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the refusal names the unresolvable member rather than shrugging.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("deaddeaddead (not known to this repository)"),
        "{stdout}"
    );

    // And when: the drop is stated, the first cut proceeds.
    let allowed = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
}

#[test]
fn a_member_merged_upstream_past_the_candidates_base_is_dropped_not_carried() {
    // Given: alpha merged into upstream by a MERGE COMMIT, so the recorded tip
    // is an ancestor of the trunk. The composition was never rebased onto the
    // landing, and a hand rebuild then dropped alpha: the trunk carries the
    // content, this cut does not. A fork-point replay would degenerate to the
    // member itself and read empty without consulting the candidate at all.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    lab.fetch_work();
    lab.jj_work(["new", "feat/beta", "-m", "hand-rebuilt without alpha"]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);

    // When: the next cut is taken without stating the drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused and names alpha — content the upstream carries is
    // not content this cut ships.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("refusing to cut") && stdout.contains("feat/alpha@"),
        "{stdout}"
    );
}

#[test]
fn an_unverified_member_stays_in_the_recorded_composition() {
    // Given: three members entangled in one file, so every cut is conflicted,
    // then a hand rebuild without alpha. Alpha's replay onto the conflicted
    // candidate answers nothing either way — and an unanswered question must
    // not fall out of the baseline, or one conflicted cut launders a member
    // out of the composition without anyone stating the drop.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.branch("feat/gamma", "shared.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first conflicted cut was refused: {first:?}"
    );
    let alpha = commit_at(&lab, "feat/alpha");
    lab.jj_work([
        "new",
        "feat/beta",
        "feat/gamma",
        "-m",
        "hand-rebuilt without alpha",
    ]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);

    // When: the next cut is taken.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the check is inconclusive rather than a refusal, and the new cut's
    // event keeps alpha in evidence so the next gate rechecks it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
    assert!(stdout.contains("carry check inconclusive"), "{stdout}");
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event.text.contains("unverified: feat/alpha@"),
        "was: {}",
        event.text
    );
    assert!(
        event.evidence.iter().any(|sha| sha == alpha.as_str()),
        "alpha fell out of the baseline: {:?}",
        event.evidence
    );
}

#[test]
fn git_tracking_refs_are_not_carriers_but_other_branches_are() {
    // Given: a maintainer branch carrying our tip and jj's matching git-tracking ref.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    // When: carriers of our tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");

    // Then: the real branch remains useful evidence, but jj's duplicate does not.
    assert!(
        carriers.contains(&BookmarkRef::Local(BranchName::new("theirs/rework"))),
        "the ordinary carrier was lost: {carriers:?}"
    );
    assert!(
        !carriers.iter().any(|reference| {
            matches!(reference, BookmarkRef::Remote { remote, .. } if remote.as_str() == "git")
        }),
        "git-tracking refs were reported: {carriers:?}"
    );
}

#[test]
fn fetched_pull_request_heads_are_not_carriers() {
    // Given: a fetched pull-request head that descends from our branch tip.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    lab.jj_work(["bookmark", "create", "pr-4545", "-r", "feat/alpha"]);
    lab.jj_work(["new", "pr-4545", "-m", "fetched pull head advance"]);
    std::fs::write(lab.work.join("pull-head.txt"), "fetched\n").expect("write pull head");
    lab.jj_work(["bookmark", "set", "pr-4545", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: carriers of the feature tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated, "origin")
        .expect("carriers");

    // Then: our fetched pull request is not mistaken for someone else's carrier.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("pr-4545"))),
        "fetched pull head was reported: {carriers:?}"
    );
}

#[test]
fn sync_fails_closed_when_the_facts_batch_fails() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = sync_entry(&lab);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(42, "OPEN", "feat/alpha", "head-42"),
        )]),
        fail_facts: true,
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("store");
    let scribe = knives::ledger::Scribe::new(
        knives::ledger::Ledger::at(state.path().join("ledger")),
        name,
        lab.work,
        "a-test".to_owned(),
    );

    let report = sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("sync report");

    assert!(report.rows.is_empty(), "was: {report:?}");
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("pull request state unavailable")),
        "was: {report:?}"
    );
    assert_eq!(sync::exit_for(&report), knives::cli::Exit::Incomplete);
}

#[test]
fn a_listed_state_wins_and_a_vanished_number_is_answered_by_the_batch() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 42);
    lab.publish_pull("feat/alpha", 43);
    let name = knives::ids::RepoName::new("demo");
    let entry = sync_entry(&lab);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(42, "OPEN", "feat/alpha", "head-42"),
        )]),
        vanished_states: BTreeMap::from([(42, "MERGED".to_owned()), (43, "MERGED".to_owned())]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("store");
    store.record_pull_head(&name, 43, "previous");
    let scribe = knives::ledger::Scribe::new(
        knives::ledger::Ledger::at(state.path().join("ledger")),
        name,
        lab.work,
        "a-test".to_owned(),
    );

    let report = sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("sync report");

    assert_eq!(
        report
            .rows
            .iter()
            .find(|row| row.number == 42)
            .map(|row| row.state),
        Some(sync::PullState::New),
        "the listed OPEN state was overwritten: {report:?}"
    );
    assert_eq!(
        report
            .rows
            .iter()
            .find(|row| row.number == 43)
            .map(|row| row.state),
        Some(sync::PullState::Merged),
        "the vanished pull did not arrive in the one batch: {report:?}"
    );
    assert!(report.problems.is_empty(), "was: {report:?}");
}

#[test]
fn sync_records_one_event_for_each_pull_request_that_moved() {
    // Given: three tracked pull requests that moved and one that did not.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state) in [
        (10, "feat/merged", "MERGED"),
        (11, "feat/closed", "CLOSED"),
        (12, "feat/moved", "OPEN"),
        (13, "feat/still", "OPEN"),
    ] {
        let _ = pull_requests.insert(
            BranchName::new(branch),
            PullRequest {
                head_ref_oid: format!("head-{number}"),
                updated_at: "2026-08-15T00:00:00Z".to_owned(),
                ..pulls::pull_request(number, state, branch)
            },
        );
    }
    let forge = knives::forge::fake::FakeForge {
        pull_requests,
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.record_pull_head(&name, 12, "older");
    store.record_pull_head(&name, 13, "head-13");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work,
        "ses_fff688".to_owned(),
    );

    // When: sync classifies them.
    let report = sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("sync report");
    assert_eq!(report.rows.len(), 4, "was: {report:?}");

    // Then: exactly the moved pulls are events, each under the tracked branch.
    let entries = ledger.entries().expect("read ledger");
    let recorded: Vec<(Option<&str>, &str)> = entries
        .iter()
        .map(|entry| (entry.subject.as_deref(), entry.text.as_str()))
        .collect();
    assert_eq!(
        recorded,
        [
            (Some("feat/merged"), "#10 merged"),
            (Some("feat/closed"), "#11 closed"),
            (Some("feat/moved"), "#12 advanced to head-12"),
        ],
        "was: {entries:?}"
    );
    assert!(entries.iter().all(|entry| entry.owner == "ses_fff688"));
    assert!(
        entries
            .iter()
            .all(|entry| entry.kind == knives::ledger::Kind::Event),
        "sync observed these; it did not assert them"
    );
}

#[test]
fn sync_records_a_settled_pull_request_once_across_repeated_runs() {
    // Given: a merged pull request that remains listed by the forge.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = sync_entry(&lab);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(10, "MERGED", "feat/alpha", "head-10"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe =
        knives::ledger::Scribe::new(ledger.clone(), name, lab.work, "ses_fff688".to_owned());

    // When: the same settled pull request is seen twice.
    sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("first sync");
    sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("second sync");

    // Then: its settled transition remains one fact, not one fact per sync run.
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.text == "#10 merged")
            .count(),
        1,
        "was: {entries:?}"
    );
}

#[test]
fn sync_records_an_advanced_pull_request_then_its_merge() {
    // Given: a tracked pull request whose head advanced before the forge reports it merged.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = sync_entry(&lab);
    let advanced = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(12, "OPEN", "feat/alpha", "head-12"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let merged = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(12, "MERGED", "feat/alpha", "head-12"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.record_pull_head(&name, 12, "older");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe =
        knives::ledger::Scribe::new(ledger.clone(), name, lab.work, "ses_fff688".to_owned());

    // When: the head advances, then the forge marks that same pull request merged.
    sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&advanced),
        scribe: &scribe,
        cache: None,
    })
    .expect("advanced sync");
    sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&merged),
        scribe: &scribe,
        cache: None,
    })
    .expect("merged sync");

    // Then: both distinct transitions remain in the ledger in observation order.
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["#12 advanced to head-12", "#12 merged"],
        "was: {entries:?}"
    );
}

#[test]
fn sync_records_each_consecutive_advance() {
    // Given: an open pull request whose head changes twice between sync runs.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = sync_entry(&lab);
    let first_advance = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(12, "OPEN", "feat/alpha", "head-b"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let second_advance = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            sync_pull_request(12, "OPEN", "feat/alpha", "head-c"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.record_pull_head(&name, 12, "head-a");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe =
        knives::ledger::Scribe::new(ledger.clone(), name, lab.work, "ses_fff688".to_owned());

    // When: the pull request advances from A to B, then from B to C.
    sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&first_advance),
        scribe: &scribe,
        cache: None,
    })
    .expect("first advance");
    sync::sync_repo(sync::SyncInput {
        entry: &entry,
        store: &mut store,
        forge: Some(&second_advance),
        scribe: &scribe,
        cache: None,
    })
    .expect("second advance");

    // Then: both changed heads are recorded as distinct advances.
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["#12 advanced to head-b", "#12 advanced to head-c"],
        "was: {entries:?}"
    );
}

#[test]
fn bookmark_tips_keeps_local_and_remote_refs_distinct() {
    // Given: a fork checkout with the same branch name locally and on origin.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.rewrite_local_branch("feature", "rewritten\n");

    // When: typed bookmark tips are read through jj-lib.
    let tips = Repo::open(&lab.work)
        .expect("open repository")
        .bookmark_tips()
        .expect("read tips");

    // Then: the local and remote references remain separate map keys.
    let local = BookmarkRef::Local(BranchName::new("feature"));
    let remote = BookmarkRef::Remote {
        branch: BranchName::new("feature"),
        remote: RemoteName::new("origin"),
    };
    assert_ne!(tips.get(&local), tips.get(&remote));
}

#[test]
fn parents_of_octopus_includes_bookmarks_for_every_parent() {
    // Given: an octopus merge over two labelled branches and main.
    let lab = lab::Lab::new();
    lab.branch("one", "one.txt", "one\n");
    lab.branch("two", "two.txt", "two\n");
    lab.octopus("release", "one", "two");

    // When: its parents are read through jj-lib.
    let parents = Repo::open(&lab.work)
        .expect("open repository")
        .parents_of("release")
        .expect("read parents");

    // Then: every octopus parent retains its bookmark reference.
    assert_eq!(parents.len(), 3);
    assert!(parents.iter().any(|parent| {
        parent
            .bookmarks
            .contains(&BookmarkRef::Local(BranchName::new("one")))
    }));
    assert!(parents.iter().any(|parent| {
        parent
            .bookmarks
            .contains(&BookmarkRef::Local(BranchName::new("two")))
    }));
}

#[test]
fn squash_merge_lands_content_that_ancestry_cannot_see() {
    // The reason this crate probes instead of asking about ancestry. Both halves
    // are asserted, and the second goes through the crate, so a regression in
    // `probe_landed` fails this test rather than only the environment fact.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    lab.fetch_work();

    // Ancestry says the branch is not in the trunk.
    let merged_in = lab.revision(
        &lab.work,
        "feat/alpha & ::main@upstream",
        "commit_id ++ \"\\n\"",
    );
    assert!(
        merged_in.trim().is_empty(),
        "the branch became an ancestor; the premise is gone"
    );

    // The crate says its content is there anyway.
    let verdict = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/alpha"),
        "main@upstream",
    )
    .expect("probe");
    assert_eq!(verdict, knives::detect::RebaseOutcome::Empty);
}

#[test]
fn probe_landed_is_empty_after_plain_squash_merge() {
    // Given: a branch squash-merged unchanged upstream.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 8);
    lab.squash_merge_pull(8, None);

    // When: the branch is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: jj reports that replay as empty.
    assert_eq!(outcome, RebaseOutcome::Empty);
}

#[test]
fn probe_landed_is_conflicted_after_maintainer_changes_squash_content() {
    // Given: a branch squash-merged with maintainer edits.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 9);
    lab.squash_merge_pull(9, Some("maintainer rewrite\n"));

    // When: the branch is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: jj preserves the maintainer conflict.
    assert_eq!(outcome, RebaseOutcome::Conflicted);
}

#[test]
fn probe_landed_is_clean_nonempty_for_open_branch() {
    // Given: an open branch not present upstream.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.advance_upstream("advance\n");
    lab.rebase_and_force_push("feature");

    // When: it is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: the replay is clean but still carries work.
    assert_eq!(outcome, RebaseOutcome::CleanNonEmpty);
}

#[test]
fn jj_lib_answers_the_same_probe_from_many_threads_as_from_one() {
    // Every parallel landed probe opens its own repository handle and replays
    // inside a transaction it drops. jj's own model is concurrent-safe by design,
    // but the loaded-repo handle is not assumed Sync, so this is measured rather
    // than believed. The operation log must also remain unchanged after all
    // concurrent probes complete.
    let lab = lab::Lab::new();
    for index in 0..8 {
        lab.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    let branches: Vec<BranchName> = (0..8)
        .map(|index| BranchName::new(format!("feat/b{index}")))
        .collect();

    // When: the same probes run serially and then all at once
    let serial: Vec<RebaseOutcome> = branches
        .iter()
        .map(|branch| probe_landed(&lab.work, branch, "main@upstream").expect("serial probe"))
        .collect();
    let work = lab.work.as_path();
    let operations_before = operation_ids(&lab.work);
    let concurrent: Vec<RebaseOutcome> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(branches.len());
        for branch in &branches {
            handles.push(scope.spawn(move || {
                probe_landed(work, branch, "main@upstream").expect("concurrent probe")
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a probe thread panicked"))
            .collect()
    });

    // Then: every answer is identical and in the same order
    assert_eq!(
        concurrent, serial,
        "a concurrent probe answered differently from a serial one"
    );
    assert!(
        serial
            .iter()
            .all(|outcome| *outcome == RebaseOutcome::CleanNonEmpty),
        "the fixture's unmerged branches should all be unlanded: {serial:?}"
    );
    // And: concurrent probes wrote no operation into the shared log.
    assert_eq!(operation_ids(&lab.work), operations_before);

    // And: the repository is still readable afterwards and retains every branch.
    let tips = Repo::open(&lab.work)
        .expect("reopen after concurrent probes")
        .bookmark_tips()
        .expect("read tips");
    assert!(
        (0..8).all(
            |index| tips.contains_key(&BookmarkRef::Local(BranchName::new(format!(
                "feat/b{index}"
            ))))
        ),
        "a feature bookmark disappeared after concurrent probes: {tips:?}"
    );
}

#[test]
fn probe_landed_cleans_only_its_temporary_commits() {
    // Given: an open branch and a stable working copy.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    let children_before = lab.revision(&lab.work, "children(main@upstream)", "commit_id");
    let branch_before = lab.revision(&lab.work, "feature", "commit_id");
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");

    // When: landing is probed.
    probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream").expect("probe landed");

    // Then: only temporary probe commits have disappeared.
    assert_eq!(
        lab.revision(&lab.work, "children(main@upstream)", "commit_id"),
        children_before
    );
    assert_eq!(
        lab.revision(&lab.work, "feature", "commit_id"),
        branch_before
    );
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

/// Operation ids in the shared op log, newest first.
fn operation_ids(repo: &std::path::Path) -> Vec<String> {
    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ])
        .current_dir(repo)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("read op log");
    assert!(output.status.success(), "read op log");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// The newest operation's description.
fn newest_operation_description(repo: &std::path::Path) -> String {
    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "description",
        ])
        .current_dir(repo)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("read newest operation");
    assert!(output.status.success(), "read newest operation");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn probes_write_nothing_to_the_shared_op_log() {
    // Given: an open branch and upstream drift, so both probes do real replay
    // work. A probe answers a read-only question; in a repo shared by several
    // agents every operation it writes is a reconciliation point and op-log
    // noise (the shape that derailed the 2026-08-08 cut diagnosis).
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.advance_upstream("advance\n");
    let trunk = lab.revision(&lab.work, "main@upstream", "commit_id");
    let tip = lab.revision(&lab.work, "feature", "commit_id");
    let operations_before = operation_ids(&lab.work);

    // When: the landed and net-diff probes both run.
    let landed = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");
    let net = knives::jj::probe_net_diff(&lab.work, &trunk, &tip, &trunk).expect("probe net diff");

    // Then: real answers, and the op log gained nothing at all.
    assert_eq!(landed, RebaseOutcome::CleanNonEmpty);
    assert_eq!(net, RebaseOutcome::CleanNonEmpty);
    assert_eq!(operation_ids(&lab.work), operations_before);
}

#[test]
fn divergent_changes_reports_both_rewrites_after_fetch() {
    // Given: the same branch rewritten independently in two jj clones.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feature");

    // When: divergence is read through jj-lib after fetching.
    let divergent = Repo::open(&lab.work)
        .expect("open repository")
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read divergence");

    // Then: exactly one change has two visible commits.
    assert_eq!(divergent.len(), 2);
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
}

#[test]
fn divergent_changes_reports_copies_buried_under_descendants() {
    // Given: one change as two visible commits, EACH buried under a child, so
    // neither copy is a view head. This is the fleet's dominant shape — a
    // branch advanced past its rewritten ancestor while a remote-pinned chain
    // kept the old copy — and enumerating only head copies missed it (the jj
    // fork carried 74 divergent changes; 35 were reported).
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feature");
    // Bury the local rewrite: make the parked child real so it survives, then
    // bury the fetched rewrite under a child of its own.
    std::fs::write(lab.work.join("local-child.txt"), "local\n").expect("write local child");
    lab.jj_work(["describe", "-m", "child of local rewrite"]);
    lab.jj_work(["new", "feature@origin", "-m", "child of fetched rewrite"]);
    std::fs::write(lab.work.join("fetched-child.txt"), "fetched\n").expect("write fetched child");
    lab.jj_work(["status"]);

    // When: divergence is read through jj-lib.
    let divergent = Repo::open(&lab.work)
        .expect("open repository")
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read divergence");

    // Then: the buried pair is still reported.
    assert_eq!(divergent.len(), 2, "{divergent:?}");
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
}

#[test]
fn divergence_pinned_only_by_a_superseded_release_ref_is_not_reported() {
    // Given: one change as two commits, where the old copy's only visibility is a
    // remote-tracking ref we are told to ignore. This is the re-materialized
    // superseded-cut shape: bare `jj git fetch` brings such refs back forever,
    // so the reader must ignore them rather than the graph staying clean.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    let repo = Repo::open(&lab.work).expect("open");

    // Sanity: unfiltered, the divergence is visible (two commits, one change).
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("divergent unfiltered");
    assert!(
        !unfiltered.is_empty(),
        "fixture failed to create divergence"
    );

    // When: the ref holding the stale copy is ignored.
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("feat/alpha"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("divergent filtered");

    // Then: the finding is gone — the stale copy was visible only through the
    // ignored ref, so nothing else vouches for it.
    assert!(filtered.is_empty(), "still reported: {filtered:?}");
}

#[test]
fn divergence_with_only_ignored_head_keeps_copies_vouched_by_live_heads() {
    // Given: two rewrites of one change, each kept as a non-head ancestor of a
    // live head, plus a third rewrite whose sole head ref is a dated release.
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");

    std::fs::write(lab.work.join("keep-one.txt"), "keep\n").expect("write first child");
    lab.jj_work(["describe", "-m", "keep first rewrite"]);
    lab.jj_work(["new", "feat/alpha@origin", "-m", "keep second rewrite"]);
    std::fs::write(lab.work.join("keep-two.txt"), "keep\n").expect("write second child");
    lab.jj_work(["status"]);

    let status = Command::new("jj")
        .args(["edit", "--ignore-immutable", "feat/alpha"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("rewrite release head");
    assert!(status.success(), "rewrite release head");
    std::fs::write(lab.second.join("feature.txt"), "third rewrite\n").expect("write third rewrite");
    let status = Command::new("jj")
        .args(["bookmark", "create", "release/2024-01-01", "-r", "@"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create release bookmark");
    assert!(status.success(), "create release bookmark");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "release/2024-01-01",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push release bookmark");
    assert!(status.success(), "push release bookmark");
    lab.fetch_work();
    lab.jj_work(["bookmark", "forget", "--include-remotes", "feat/alpha"]);
    let repo = Repo::open(&lab.work).expect("open");

    // When: the release ref is ignored.
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read unfiltered divergence");
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("release/2024-01-01"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("read filtered divergence");

    // Then: the ignored head is excluded but its two live-vouched sibling copies remain.
    assert_eq!(unfiltered.len(), 3, "fixture should expose three copies");
    assert_eq!(
        filtered.len(),
        2,
        "live-vouched copies disappeared: {filtered:?}"
    );
    assert_eq!(filtered[0].0, filtered[1].0);
}

#[test]
fn unrelated_divergence_survives_a_nonempty_ignored_ref_set() {
    // Given: independently rewritten alpha and beta branches.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    lab.branch("feat/beta", "beta.txt", "one\n");
    lab.rewrite_in_both_clones("feat/beta");
    let repo = Repo::open(&lab.work).expect("open");

    // When: only alpha's remote ref is ignored.
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read unfiltered divergence");
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("feat/alpha"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("read filtered divergence");

    // Then: alpha is suppressed while beta's pair remains reported.
    assert_eq!(unfiltered.len(), 4, "fixture should expose two divergences");
    assert_eq!(
        filtered.len(),
        2,
        "beta divergence disappeared: {filtered:?}"
    );
    assert_eq!(filtered[0].0, filtered[1].0);
}

#[test]
fn pull_heads_reads_local_upstream_pull_refs() {
    // Given: a branch published at an upstream pull-ref namespace.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 42);

    // When: pull heads are listed through the git transport.
    let heads = pull_heads(
        &lab.work,
        lab.upstream.to_str().expect("utf-8 upstream path"),
    )
    .expect("read pull heads");

    // Then: the pull number maps to its published object id.
    assert!(heads.contains_key(&42));
}

#[test]
fn remote_refs_reads_live_heads_by_pattern() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let url = lab.temp_origin().display().to_string();

    let refs = remote_refs(&url, &["refs/heads/*"]).expect("ls-remote");

    assert!(refs.contains_key("refs/heads/feat/alpha"));
    let none = remote_refs(&url, &["refs/pull/*/head"]).expect("ls-remote");
    assert!(none.is_empty(), "a path remote has no pull refs");
}

#[test]
fn consumers_reports_stale_and_behind_locks() {
    let lab = Lab::new();
    lab.branch("release/2026-08-04", "release.txt", "first\n");
    lab.push_branch("release/2026-08-04");
    let consumer = tempfile::tempdir().expect("create consumer");
    std::fs::write(
        consumer.path().join("uv.lock"),
        "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-04#deadbeef\" }\n",
    )
    .expect("write frozen pin");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            lab.temp_origin().display(),
            consumer.path().display(),
        ),
    )
    .expect("write registry");
    let knives = || {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(["--text", "consumers", "demo"])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .output()
            .expect("run consumers")
    };

    let stale = knives();

    assert_eq!(stale.status.code(), Some(1), "stderr: {:?}", stale.stderr);
    assert!(
        String::from_utf8_lossy(&stale.stdout).contains("stale lock: expected @"),
        "stdout: {}",
        String::from_utf8_lossy(&stale.stdout)
    );

    lab.branch("release/2026-08-05", "release.txt", "second\n");
    lab.push_branch("release/2026-08-05");

    let behind = knives();

    assert_eq!(behind.status.code(), Some(1), "stderr: {:?}", behind.stderr);
    assert!(
        String::from_utf8_lossy(&behind.stdout).contains("behind: newest is release/2026-08-05"),
        "stdout: {}",
        String::from_utf8_lossy(&behind.stdout)
    );
}

#[test]
fn consumers_reports_a_missing_registry_path_as_incomplete() {
    let lab = Lab::new();
    lab.branch("release/2026-08-04", "release.txt", "first\n");
    lab.push_branch("release/2026-08-04");
    let home = tempfile::tempdir().expect("create config home");
    let missing = home.path().join("gone");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            lab.temp_origin().display(),
            missing.display(),
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "consumers", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run consumers");

    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("PROBLEM: not found"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn consumers_leaves_pins_unclassified_when_live_release_refs_fail() {
    let lab = Lab::new();
    lab.branch("release/2026-08-05", "release.txt", "published\n");
    lab.push_branch("release/2026-08-05");
    let consumer = tempfile::tempdir().expect("create consumer");
    std::fs::write(
        consumer.path().join("uv.lock"),
        "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-05#deadbeef\" }\n",
    )
    .expect("write frozen pin");
    let home = tempfile::tempdir().expect("create config home");
    let unavailable = home.path().join("unavailable-release-remote");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            unavailable.display(),
            consumer.path().display(),
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "consumers", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run consumers");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    assert!(stdout.contains("unclassified"), "stdout: {stdout}");
    assert!(
        !stdout.contains("stale lock:") && !stdout.contains("behind:"),
        "live remote failure must not derive a local verdict: {stdout}"
    );
}

#[test]
fn consumers_reports_an_unreadable_pin_file_as_incomplete() {
    let lab = Lab::new();
    lab.branch("release/2026-08-05", "release.txt", "published\n");
    lab.push_branch("release/2026-08-05");
    let consumer = tempfile::tempdir().expect("create consumer");
    std::fs::create_dir(consumer.path().join("uv.lock")).expect("create unreadable pin file");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            lab.temp_origin().display(),
            consumer.path().display(),
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "consumers", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run consumers");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    assert!(
        stdout.contains("PROBLEM: could not read uv.lock"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("does not pin demo"),
        "a failed file scan must not make a no-pin claim: {stdout}"
    );
}

#[test]
fn changed_files_reports_sorted_paths_without_snapshotting_working_copy() {
    // Given: a branch with one changed file and an untouched working copy.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");

    // When: changed paths are requested for the branch revision.
    let files = changed_files(&lab.work, "feature").expect("read changed files");

    // Then: paths are normalized, sorted, and the working copy stays untouched.
    assert_eq!(files, vec!["feature.txt"]);
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

#[test]
fn changed_files_between_handles_a_branch_behind_advanced_upstream() {
    // Given: a branch whose upstream trunk has advanced since the branch forked.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "main@upstream",
        "-m",
        "merge upstream into branch",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");
    let from = "fork_point(main@upstream | feat/alpha)";

    // When: the branch tree is compared directly with its fork point.
    let files = changed_files_between(&lab.work, from, "feat/alpha").expect("diff branch trees");

    // Then: the branch file is returned without changing the working copy.
    assert_eq!(files, vec!["alpha.txt"]);
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

#[test]
fn the_net_probe_cleans_up_its_bookmark_and_commits() {
    // Given: a multi-commit member range, which requires a bookmark-tracked synthetic probe.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "first\n");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "first\nsecond\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let before_commits = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    let before_bookmarks = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--all-remotes",
        ])
        .output()
        .expect("list bookmarks before probe");
    assert!(
        before_bookmarks.status.success(),
        "bookmark list failed: {}",
        String::from_utf8_lossy(&before_bookmarks.stderr)
    );

    // When: the net probe creates, rewrites, and cleans up its synthetic commit.
    knives::jj::probe_net_diff(&lab.work, "main@origin", "feat/alpha", "main@origin")
        .expect("probe net diff");

    // Then: both globally visible commits and bookmarks exactly match their prior state.
    let after_commits = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    let after_bookmarks = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--all-remotes",
        ])
        .output()
        .expect("list bookmarks after probe");
    assert!(
        after_bookmarks.status.success(),
        "bookmark list failed: {}",
        String::from_utf8_lossy(&after_bookmarks.stderr)
    );
    assert_eq!(
        before_commits, after_commits,
        "the net probe left commits behind"
    );
    assert_eq!(
        before_bookmarks.stdout, after_bookmarks.stdout,
        "the net probe left bookmarks behind"
    );
}

#[test]
fn the_range_probe_cleans_up_every_scratch_commit() {
    // Given: an octopus range whose duplicate creates a parent/child scratch chain.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("feat/pair", "feat/alpha", "feat/beta");
    let before = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");

    // When: the landed-range probe cleans up the commits it duplicated.
    let outcome = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/pair"),
        "main",
    );
    assert!(
        outcome.is_ok(),
        "an octopus range must be probed, not refused: {outcome:?}"
    );

    // Then: enumerating every visible commit finds no scratch-chain residue.
    let after = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    assert_eq!(before, after, "the range probe left commits behind");
}

#[test]
fn a_new_workspace_is_based_on_the_upstream_trunk_not_the_current_change() {
    // The accident this default exists to prevent: an agent sitting in a release
    // workspace runs `jj new` and silently inherits the release merge as a
    // parent, so unrelated work rides into its pull request.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/dated", "feat/alpha", "feat/beta");
    // Move the upstream trunk so it differs from our fork's copy. Without this
    // the two are the same commit and the test passes whichever trunk the code
    // uses, proving nothing.
    lab.advance_upstream("moved on\n");

    // Given: the working copy is parked on the release merge, the dangerous spot
    let parked = lab.revision(&lab.work, "@", "change_id.short(8)");
    assert!(!parked.trim().is_empty());

    // When: a workspace is opened the way `knives start` opens one
    let destination = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &destination, "main@upstream")
        .expect("add workspace");

    // Then: its only parent is the upstream trunk, not the release merge
    let parents = lab.revision(&destination, "parents(@)", "commit_id.short(12) ++ \"\\n\"");
    let upstream = lab.revision(&lab.work, "main@upstream", "commit_id.short(12)");
    let listed: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(
        listed,
        vec![upstream.trim()],
        "new work must sit on the upstream trunk alone"
    );
}

#[test]
fn start_bases_a_new_branch_on_the_shared_base_not_the_advanced_upstream() {
    // Given: a release whose members fork from today's trunk, then upstream advances.
    // Basing new work on the advanced tip would drag that advance into the next
    // cut through one member — the mixed-base conflict storm (#10).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let repo = Repo::open(&lab.work).expect("open");
    let base_before_advance = repo.resolve_commit("main@origin").expect("resolve base");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "test",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the shared base, not the advanced tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        base_before_advance.as_str(),
        "based on {parent}, expected the shared base"
    );
}

#[test]
fn start_without_a_release_uses_the_fetched_upstream_trunk() {
    // Given: a registry with no release and an upstream tip distinct from origin.
    let lab = lab::Lab::new();
    lab.advance_upstream("upstream advance\n");
    let upstream_trunk = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@upstream")
        .expect("resolve upstream trunk");
    let (home, _consumer) = release_test_home(&lab);

    // When: the binary starts a branch without a release base to select.
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/no-release",
            "--repo",
            "demo",
            "--why",
            "test",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: its parent is the fetched upstream trunk and the fallback is disclosed.
    let workspace = lab.work.parent().expect("parent").join("feat-no-release");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(parent, upstream_trunk.as_str());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("(the fetched upstream trunk)"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn start_resumes_the_same_harness_sessions_claim_without_mutating_it() {
    // A second invocation from the same harness session must acknowledge the
    // existing claim rather than overwrite its timestamp or reason.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("KNIVES_OWNER", "agent-one")
            .output()
            .expect("run start")
    };

    let first = run(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--why",
        "port it",
    ]);
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let state_before = std::fs::read_to_string(home.path().join("state.json")).expect("state");

    let second = run(&["--text", "start", "feat/gamma", "--repo", "demo"]);

    assert!(
        second.status.success(),
        "resume must exit 0: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("resumed"), "stdout: {stdout}");
    assert!(stdout.contains("feat-gamma"), "stdout: {stdout}");
    assert_eq!(
        std::fs::read_to_string(home.path().join("state.json")).expect("state"),
        state_before,
        "resume must not rewrite the claim"
    );

    let events = run(&[
        "--text",
        "notch",
        "feat/gamma",
        "--events",
        "--repo",
        "demo",
    ]);
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    assert!(
        String::from_utf8_lossy(&events.stdout).contains("resumed"),
        "events: {}",
        String::from_utf8_lossy(&events.stdout)
    );
}

#[test]
fn start_refuses_two_anonymous_owners_with_the_same_name() {
    // Equal OS-user strings are not a trustworthy identity proof, so the second
    // anonymous terminal must receive the claim context and an explicit override.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let outside = tempfile::tempdir().expect("create unmanaged terminal");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(outside.path())
            .env("KNIVES_CONFIG_HOME", home.path())
            .env_remove("KNIVES_OWNER")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env("USER", "terminal-user")
            .output()
            .expect("run start")
    };

    let first = run(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--why",
        "port it",
    ]);
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run(&["--text", "start", "feat/gamma", "--repo", "demo"]);

    assert_eq!(second.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&second.stderr));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("anonymous"), "stderr: {stderr}");
    assert!(stderr.contains("port it"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
}

#[test]
fn start_refuses_another_harness_session_and_names_the_holder() {
    // A different harness identity must not inherit the first agent's workspace
    // silently; the refusal names enough context to make the override auditable.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("KNIVES_OWNER", owner)
            .output()
            .expect("run start")
    };

    let first = run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run(
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    assert_eq!(second.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&second.stderr));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert!(stderr.contains("harness-session"), "stderr: {stderr}");
    assert!(stderr.contains("claimed"), "stderr: {stderr}");
    assert!(stderr.contains("last seen"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
}

#[test]
fn start_from_inside_the_claimed_workspace_resumes_by_possession() {
    // Possession is intentionally weaker than a harness identity and must leave
    // its own ledger trail instead of mutating the held claim.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let first = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("first start");
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");

    let second = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "start", "feat/gamma", "--repo", "demo"])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env_remove("KNIVES_OWNER")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env("USER", "terminal-user")
        .output()
        .expect("resume from workspace");

    assert!(
        second.status.success(),
        "possession resume failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("possession"),
        "stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let events = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("read events");
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    assert!(
        String::from_utf8_lossy(&events.stdout).contains("resumed via workspace possession"),
        "events: {}",
        String::from_utf8_lossy(&events.stdout)
    );
}

#[test]
fn start_force_seizes_and_records_the_previous_owner() {
    // A force seizure preserves the workspace and records both the displaced
    // identity and the new reason in the durable event stream.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("KNIVES_OWNER", owner)
            .output()
            .expect("run start")
    };
    let first = run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let change = lab.revision(&workspace, "@", "change_id.short(12)");

    let second = run(
        "agent-two",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );

    assert!(
        second.status.success(),
        "force start failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-two".to_owned())
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains(change.trim()),
        "stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let events = run(
        "agent-two",
        &[
            "--text",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ],
    );
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    let events = String::from_utf8_lossy(&events.stdout);
    assert!(
        events.contains("seized from agent-one (harness-session"),
        "events: {events}"
    );
    assert!(events.contains("rescue stalled work"), "events: {events}");
}

#[test]
fn start_adopts_an_existing_workspace_for_an_unclaimed_branch() {
    // A workspace made outside knives is still valid work to claim. The command
    // must reuse it rather than dying on the destination's existence.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &workspace, "main@upstream")
        .expect("create existing workspace");
    let change = lab.revision(&workspace, "@", "change_id.short(12)");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "adopt it",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("adopt workspace");

    assert!(
        output.status.success(),
        "adopt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("adopted"), "stdout: {stdout}");
    assert!(stdout.contains("left as-is"), "stdout: {stdout}");
    assert!(stdout.contains(change.trim()), "stdout: {stdout}");
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-one".to_owned())
    );
}

#[test]
fn start_adopts_a_no_cleanup_forgotten_workspace_without_resetting_it() {
    // `finish --no-cleanup` intentionally keeps the directory, but forgets its
    // registration. Starting it again must reattach that exact working copy.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run_start = |why: &str| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args([
                "--text",
                "start",
                "feat/gamma",
                "--repo",
                "demo",
                "--why",
                why,
            ])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("KNIVES_OWNER", "agent-one")
            .output()
            .expect("run start")
    };
    let started = run_start("port it");
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let work_file = workspace.join("in-progress.txt");
    std::fs::write(&work_file, "preserve this work\n").expect("write in-progress work");
    let change_before = lab.revision(&workspace, "@", "change_id");

    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--allow-open",
            "--no-cleanup",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("finish without cleanup");
    assert!(
        finished.status.success(),
        "finish failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(workspace.is_dir(), "workspace directory was removed");

    let restarted = run_start("resume preserved work");

    assert!(
        restarted.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert!(
        String::from_utf8_lossy(&restarted.stdout).contains("adopted"),
        "stdout: {}",
        String::from_utf8_lossy(&restarted.stdout)
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-one".to_owned())
    );
    assert_eq!(
        lab.revision(&workspace, "@", "change_id"),
        change_before,
        "adoption reset the working-copy change"
    );
    assert_eq!(
        std::fs::read_to_string(&work_file).expect("read preserved work"),
        "preserve this work\n"
    );
}

#[test]
fn start_force_without_why_is_a_usage_error() {
    // Clap owns this validation so a force never reaches claim handling without
    // a durable human explanation.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("parse start");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn shared_base_selects_the_newest_of_multiple_trunk_reachable_release_parents() {
    // Given: a release carrying an old origin trunk parent, a newer upstream trunk
    // parent, and a feature parent. This is the accumulated-bases shape #11 leaves
    // behind after upstream advances.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-m",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    let repo = Repo::open(&lab.work).expect("open");
    let release = repo
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let newest_trunk_parent = repo
        .resolve_commit("main@upstream")
        .expect("resolve upstream trunk");

    // When: the release's shared base is selected.
    let shared_base = knives::commands::release::shared_base(&repo, &release, &newest_trunk_parent)
        .expect("select shared base");

    // Then: the newer trunk parent wins, not the older accumulation residue.
    assert_eq!(shared_base, Some(newest_trunk_parent));
}

#[test]
fn a_flat_releases_shared_base_is_the_members_fork_point() {
    // Given: a doctrine-flat release — members only, the base never a parent —
    // and an upstream that has advanced past the members' fork point.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "flat release",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let fork_point = repo
        .resolve_commit("main@origin")
        .expect("resolve fork point");
    lab.advance_upstream("upstream advance\n");
    let reopened = Repo::open(&lab.work).expect("reopen");
    let release = reopened
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let trunk_tip = reopened
        .resolve_commit("main@upstream")
        .expect("resolve trunk tip");

    // When: the release's shared base is selected.
    let shared_base = knives::commands::release::shared_base(&reopened, &release, &trunk_tip)
        .expect("select shared base");

    // Then: it is the commit every member forks from, not the advanced tip and
    // not nothing — a flat release still has exactly one fork point.
    assert_eq!(shared_base, Some(fork_point));
}

#[test]
fn a_flat_release_recuts_after_upstream_drift_collides_with_a_member() {
    // Given: a flat two-member cut, then upstream lands a squash that rewrites
    // the same file alpha creates. Auditing against the trunk tip instead of
    // the fork point made innocent beta read as diverging: its synthetic diff
    // deletes the drifted file while the cut modifies it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.publish_pull("feat/gamma", 9);
    lab.squash_merge_pull(9, Some("upstream drift\n"));

    // When: the identical composition is re-cut under the new name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut lands — each member's content is measured from the fork
    // point, so upstream drift is not charged to the members. (The overall exit
    // still reports the unrelated lagging-trunk finding; only the audit is under
    // test here.)
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "audit false positive: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-05"),
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "the re-cut must carry the composition verbatim"
    );
}

#[test]
fn a_recut_carries_a_recorded_resolution_that_dropped_content() {
    // Given: two members conflicting on one file, the conflict resolved by hand
    // ON the release with content that matches neither side — a published
    // judgment that deliberately diverges from both members.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first cut was refused: {first:?}"
    );
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When: the identical composition is re-cut under a new name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the recorded resolution is carried, reported, and not refused —
    // the audit charges a cut only with divergence it introduces.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "a recorded resolution was refused as a loss: {stdout}"
    );
    assert!(
        stdout.contains(
            "feat/alpha: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        ) && stdout.contains(
            "feat/beta: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        ),
        "missing carried-resolution report: {stdout}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "shared.txt"),
        "merged\n",
        "the resolution must survive the re-cut"
    );
}

#[test]
fn members_counts_parents_and_names_their_holders() {
    // Given: a flat two-member cut, then feat/alpha advances without moving
    // the release parent it originally held.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let release = commit_at(&lab, "release/2026-08-04");
    let released_alpha = commit_at(&lab, "feat/alpha");
    let released_beta = commit_at(&lab, "feat/beta");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced_alpha = commit_at(&lab, "feat/alpha");

    // When: the release's members are inspected through the real CLI.
    let output = knives_release(&lab, &home, &["members"]);

    // Then: its own two direct parents are counted, and each is represented
    // once by its current holder or its branch's advanced successor.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "release/2026-08-04 @ {} — 2 parents",
            release.as_str().chars().take(12).collect::<String>()
        )),
        "the count must come from the release's parent list: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "- {} feat/beta",
            released_beta.as_str().chars().take(12).collect::<String>()
        )),
        "the held parent is missing its holder: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "- {} feat/alpha advanced to {}",
            released_alpha.as_str().chars().take(12).collect::<String>(),
            advanced_alpha.as_str().chars().take(12).collect::<String>()
        )),
        "the advanced member must name the current tip: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with("- ")).count(),
        2,
        "each release parent must render exactly one row: {stdout}"
    );
}

#[test]
fn members_verify_reports_a_dropped_members_content() {
    // Given: the same hand-resolved conflicting cut as the recut scenario,
    // where the release tree contains neither member's original content.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first cut was refused: {first:?}"
    );
    let dropped_beta = commit_at(&lab, "feat/beta");
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When: each member's content is replayed against the resolved release.
    let output = knives_release(&lab, &home, &["members", "--verify"]);

    // Then: the lost member is a finding, rather than a successful inspection.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "members verify must fail closed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "!! feat/beta@{}: the cut tree is missing or diverges from the member's content",
            dropped_beta.as_str().chars().take(12).collect::<String>()
        )),
        "the dropped member must be named under missing: {stdout}"
    );
}

#[test]
fn prose_parent_lines_do_not_inflate_the_count() {
    // Given: a flat two-member release whose description contains a line that
    // looks like an old text parser's parent record.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work([
        "describe",
        "-r",
        "release/2026-08-04",
        "-m",
        "release notes\nparent deadbeef from feat/x",
    ]);

    // When: the release is inspected by name.
    let output = knives_release(&lab, &home, &["members", "release/2026-08-04"]);

    // Then: prose never changes the structural parent count.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("release/2026-08-04 @") && stdout.contains("— 2 parents"),
        "the prose line inflated the count: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with("- ")).count(),
        2,
        "the report must have exactly one row per actual parent: {stdout}"
    );
}

#[test]
fn a_first_cut_audits_members_from_their_fork_point_not_the_trunk_tip() {
    // Given: three branches forked from the seed, one of them squash-merged
    // upstream with maintainer edits that rewrite alpha's file. The first cut
    // has no previous composition, so its audit base is chosen from scratch.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/gamma", 9);
    lab.squash_merge_pull(9, Some("upstream drift\n"));

    // When: the first cut merges every branch.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: it succeeds, flat, with exactly the three branches as parents.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "first cut failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 3, "parents: {parents:?}");
    assert!(
        !parents.contains(&commit_at(&lab, "main@upstream")),
        "the trunk must not be a parent"
    );
}

#[test]
fn start_bases_a_new_branch_on_a_flat_releases_fork_point() {
    // Given: a doctrine-flat release — no trunk parent to find — and an
    // upstream that has advanced past the members' fork point.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "flat release",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let fork_point = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@origin")
        .expect("resolve fork point");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "test",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the members' fork point, not the tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        fork_point.as_str(),
        "based on {parent}, expected the flat release's fork point"
    );
}

#[test]
fn starting_and_finishing_a_branch_leaves_its_reason_in_the_ledger() {
    // Given: a managed fork and a config home
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary with a reason
    let started = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/alpha",
            "--repo",
            "demo",
            "--why",
            "carrying the queue fix",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run start");
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    // Then: the ledger holds the claim event. `start` opens a workspace at the
    // base revision and does not create a bookmark, so only the Scribe may decide
    // whether a ref anchor exists; here it correctly records none.
    let ledger = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"));
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].kind, knives::ledger::Kind::Event);
    assert_eq!(entries[0].owner, "ses_fff688");
    assert_eq!(entries[0].subject.as_deref(), Some("feat/alpha"));
    assert_eq!(entries[0].text, "claimed: carrying the queue fix");
    assert_eq!(entries[0].anchor, None);

    // When: it is handed back naming its successor
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/alpha",
            "--allow-open",
            "--repo",
            "demo",
            "--superseded-by",
            "feat/replacement",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run finish");
    assert!(
        finished.status.success(),
        "finish failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );

    // Then: the supersession is recorded as an event rather than only as state
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(entries.len(), 2, "was: {entries:?}");
    assert_eq!(
        entries[1].text,
        "claim released; superseded by feat/replacement"
    );
}

/// A claim written straight into the store, so a `finish` test starts from a held
/// branch without `start` putting its own event in the ledger first.
fn hold_claim(home: &tempfile::TempDir, branch: &str) {
    let mut store = Store::open_for_update(home.path().join("state.json")).expect("open store");
    let _ = store.claim(
        &knives::ids::BranchTarget::new(
            knives::ids::RepoName::new("demo"),
            BranchName::new(branch),
        ),
        &knives::commands::claim::Identity {
            owner: "ses_fff688".to_owned(),
            kind: OwnerKind::HarnessSession,
        },
        "carrying the queue fix",
    );
    store.save().expect("save store");
}

fn knives_finish(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "finish"]);
    command.args(args);
    command
        .args(["--allow-open", "--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run finish")
}

#[derive(Clone, Copy)]
struct FinishWithSnapshotForgeInput<'a> {
    lab: &'a Lab,
    home: &'a tempfile::TempDir,
    pulls: &'a str,
    withheld_facts: &'a [u64],
    args: &'a [&'a str],
    log: &'a std::path::Path,
}

fn knives_finish_with_snapshot_forge(
    input: FinishWithSnapshotForgeInput<'_>,
) -> std::process::Output {
    let FinishWithSnapshotForgeInput {
        lab,
        home,
        pulls,
        withheld_facts,
        args,
        log,
    } = input;
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh(shim.path(), pulls, withheld_facts, Some(log));
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "finish"]);
    command.args(args);
    command
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run finish with a forge shim")
}

fn knives_finish_with_failing_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    args: &[&str],
    log: &std::path::Path,
) -> std::process::Output {
    let shim = tempfile::tempdir().expect("create failing forge shim directory");
    install_failing_gh(shim.path(), log);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "finish"]);
    command.args(args);
    command
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run finish with a failing forge shim")
}

#[test]
fn finish_refuses_while_the_pull_request_is_open() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");
    let state = tempfile::tempdir().expect("test state");
    let log = state.path().join("gh.log");
    let pulls = format!("[{}]", pull_record(7, "OPEN", "feat/alpha", None));

    let output = knives_finish_with_snapshot_forge(FinishWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[],
        args: &["feat/alpha"],
        log: &log,
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("open pull request #7"),
        "the refusal did not name the open pull request: {stdout}"
    );
    assert!(log.is_file(), "the guard did not consult the fake forge");
}

#[test]
fn finish_refuses_when_the_forge_omits_a_surfaced_pull_fact() {
    // Given: discovery says alpha owns an open pull request, but the same run's
    // requested-facts batch omits that number.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");
    let state = tempfile::tempdir().expect("test state");
    let log = state.path().join("gh.log");
    let pulls = format!("[{}]", pull_record(7, "OPEN", "feat/alpha", None));

    let output = knives_finish_with_snapshot_forge(FinishWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[7],
        args: &["feat/alpha"],
        log: &log,
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("cannot verify whether feat/alpha has an open pull request")
            && stdout.contains("#7"),
        "the refusal did not name the unanswered pull request: {stdout}"
    );
}

#[test]
fn finish_refuses_when_it_cannot_verify_and_allow_open_proceeds() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");
    let state = tempfile::tempdir().expect("test state");
    let refusal_log = state.path().join("refusal-gh.log");

    let refused = knives_finish_with_failing_forge(&lab, &home, &["feat/alpha"], &refusal_log);

    let refusal = String::from_utf8_lossy(&refused.stdout);
    assert_eq!(
        refused.status.code(),
        Some(3),
        "stdout: {refusal}\nstderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        refusal.contains("cannot verify whether feat/alpha has an open pull request"),
        "missing verification refusal: {refusal}"
    );
    assert!(refusal_log.is_file(), "the failed guard never reached gh");

    let bypass_log = state.path().join("bypass-gh.log");
    let bypass =
        knives_finish_with_failing_forge(&lab, &home, &["feat/alpha", "--allow-open"], &bypass_log);

    let stdout = String::from_utf8_lossy(&bypass.stdout);
    assert!(
        bypass.status.success(),
        "allow-open did not release the claim: {stdout}\n{}",
        String::from_utf8_lossy(&bypass.stderr)
    );
    assert!(stdout.contains("claim released"), "was: {stdout}");
    assert!(
        !bypass_log.exists(),
        "--allow-open spawned the fake gh: {}",
        std::fs::read_to_string(&bypass_log).unwrap_or_default()
    );
}

#[test]
fn finishing_a_held_branch_without_a_successor_records_only_the_release() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");

    let finished = knives_finish(&lab, &home, &["feat/alpha"]);
    assert!(finished.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].text, "claim released");
}

#[test]
fn finishing_a_branch_nobody_held_records_no_release_that_never_happened() {
    // The ledger is the one record meant to be trusted months later, and an event
    // is a past-tense fact this tool observed. `finish` on an unheld branch
    // releases nothing — the command's own prose already says "was not held" —
    // so an entry claiming a release is a fabrication in the audit trail.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let finished = knives_finish(&lab, &home, &["feat/alpha"]);
    assert!(finished.status.success());
    assert!(
        String::from_utf8_lossy(&finished.stdout).contains("was not held"),
        "was: {}",
        String::from_utf8_lossy(&finished.stdout)
    );

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert!(
        entries.is_empty(),
        "a release that never happened: {entries:?}"
    );
}

#[test]
fn finishing_an_unheld_branch_still_records_the_supersession_it_did_record() {
    // Two acts, and either can happen alone: `--superseded-by` writes a
    // supersession into the store whether or not a claim was held, so the entry
    // says that and not the release that did not happen.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let finished = knives_finish(
        &lab,
        &home,
        &["feat/alpha", "--superseded-by", "feat/replacement"],
    );
    assert!(finished.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].text, "superseded by feat/replacement");
}

#[test]
fn stating_a_pull_request_and_a_dependency_leaves_both_statements_in_the_ledger() {
    // Given: a managed fork with a branch, and a sibling repo to depend on
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    let sibling = home.path().join("sibling");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             [repos.sibling]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/other.git\"\n",
            lab.work.display(),
            lab.upstream.display(),
            sibling.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write registry");
    let knives = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("KNIVES_OWNER", "ses_fff688")
            .output()
            .expect("run knives")
    };

    // When: the branch's pull request is stated, then a dependency, then the
    // statement is withdrawn
    assert!(
        knives(&["--text", "track", "feat/alpha", "--pr", "4545"])
            .status
            .success()
    );
    assert!(
        knives(&["--text", "depends", "feat/alpha", "--on", "sibling#49"])
            .status
            .success()
    );
    assert!(
        knives(&["--text", "track", "feat/alpha", "--forget"])
            .status
            .success()
    );

    // Then: all three statements are in order, anchored, and the stated pull
    // request is stamped on the entries written while it was stated
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let texts: Vec<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "stated as #4545",
            "requires sibling#49",
            "pull request statement forgotten"
        ],
        "was: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.subject.as_deref() == Some("feat/alpha"))
    );
    // Each entry is stamped with the number it is about: the one that created the
    // association, the one recorded while it stood, and the one it withdrew.
    assert_eq!(
        entries.iter().map(|entry| entry.pr).collect::<Vec<_>>(),
        [Some(4545), Some(4545), Some(4545)],
        "was: {entries:?}"
    );
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    assert!(
        entries
            .iter()
            .all(|entry| entry.anchor.as_deref() == Some(tip.as_str()))
    );

    // And: the whole chronology of that number is findable BY that number, which
    // is the only thing the stamped field is for. Stamping the pre-change value
    // on the statement event would have returned two of the three.
    let filtered = knives(&["--json", "notch", "--pr", "4545"]);
    let parsed: serde_json::Value =
        serde_json::from_slice(&filtered.stdout).expect("notch --json emits JSON");
    assert_eq!(parsed["matched"], 3, "was: {parsed}");
}

#[test]
fn a_fork_only_statement_is_recorded_as_the_decision_it_is() {
    let lab = lab::Lab::new();
    lab.branch("feat/ci-only", "ci.yml", "on: push\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "track",
            "feat/ci-only",
            "--fork-only",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run track");
    assert!(output.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "stated as having no upstream pull request");
}

#[test]
fn release_plan_exits_with_findings_when_the_current_release_lags_the_upstream_trunk() {
    // Given: a clean dated release that was cut before upstream advanced.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release plan reports its warnings.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout);

    // Then: scripts receive the findings exit code for the actionable trunk warning.
    assert!(
        text.contains("does not contain the upstream trunk"),
        "trunk lag not rendered: {text}"
    );
    assert_eq!(output.status.code(), Some(1), "stdout: {text}");
}

#[test]
fn the_base_parent_is_not_stale_and_a_drifted_member_is_a_mixed_base_finding() {
    // Given: a release whose first parent is the bookmarkless shared base, and
    // one member re-based past it onto the advanced upstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_origin_branch("main", "origin advance\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    lab.advance_upstream("upstream advance\n");
    lab.rebase_and_force_push("feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: the bookmarkless base is not reported as a stale parent.
    assert!(
        !text.contains("carries no bookmark"),
        "base parent misread as stale: {text}"
    );
    // And: the drifted member is named as a mixed base.
    assert!(
        text.contains("feat/beta") && text.contains("beyond the shared base"),
        "mixed base not reported: {text}"
    );
    // And: the member still on the base is not reported.
    assert!(
        !text.contains("feat/alpha carries"),
        "well-based member misreported: {text}"
    );
    assert_eq!(output.status.code(), Some(1), "stdout: {text}");
}

#[test]
fn older_upstream_release_parent_is_reported_as_a_superseded_base() {
    // Given: a release that accumulated the old and newer upstream-trunk positions.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let base0 = lab.revision(&lab.work, "main@upstream", "commit_id");
    lab.advance_upstream("upstream advance\n");
    let base1 = lab.revision(&lab.work, "main@upstream", "commit_id");
    lab.jj_work([
        "new",
        "-r",
        base0.trim(),
        "-r",
        base1.trim(),
        "-r",
        "feat/alpha",
        "-m",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release plan is rendered.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout);

    // Then: the obsolete trunk parent is explicitly classified for repair.
    assert!(
        text.contains("older upstream base superseded by"),
        "superseded base not reported: {text}"
    );
}

#[test]
fn preflight_renders_a_mixed_base_finding_and_exits_with_findings() {
    // Given: a release with a bookmarkless base and a member rebased past it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_origin_branch("main", "origin advance\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    lab.advance_upstream("upstream advance\n");
    lab.rebase_and_force_push("feat/beta");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let state = tempfile::tempdir().expect("create state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    let forge = knives::forge::fake::FakeForge {
        fail_facts: true,
        ..knives::forge::fake::FakeForge::default()
    };

    // When: preflight gathers and renders the repository state.
    let report = knives::commands::preflight::gather(knives::commands::preflight::GatherInput {
        name: &knives::ids::RepoName::new("demo"),
        entry: &entry,
        store: &mut store,
        forge: &forge,
        cache: None,
    });
    let text = knives::commands::preflight::render(&report);

    // Then: the finding is visible and makes the command actionable to scripts.
    assert!(
        text.contains("!!") && text.contains("beyond the shared base"),
        "mixed base not rendered: {text}"
    );
    assert_eq!(
        knives::commands::preflight::exit_for(&report),
        knives::cli::Exit::Findings
    );
}

#[test]
fn a_cut_is_flat_and_carries_its_provenance() {
    // A release must be a flat merge of exactly the parents intended. The
    // failure this guards is silent: a cut that dropped a parent looks exactly
    // like one that did not, until work goes missing downstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha, "pull/10/head".to_owned()),
            (beta, "feat/beta".to_owned()),
        ],
    };
    let created =
        knives::commands::release::cut(&lab.work, &request, &ReleaseScheme::Dated).expect("cut");

    // Flat: exactly two parents, no nested integration node.
    let parents = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .parents_of(created.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2, "a release must be flat");

    // The dated name points at it, and the provenance rode along.
    let named = lab.revision(&lab.work, "release/2026-07-30", "commit_id");
    assert_eq!(named.trim(), created.as_str());
    let message = lab.revision(&lab.work, created.as_str(), "description");
    assert!(
        message.contains("from pull/10/head"),
        "provenance was lost: {message}"
    );
}

#[test]
fn a_cut_refuses_when_the_merge_did_not_get_the_parents_it_asked_for() {
    // The refusal was untested: the whole ensure! block could be deleted and the
    // flatness test still passed. jj dedupes duplicate parents, so asking for
    // the same commit twice produces a merge with fewer parents than requested,
    // which is exactly the "a branch's work was dropped" shape.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), alpha.clone()],
        provenance: vec![(alpha, "feat/alpha".to_owned())],
    };
    let outcome = knives::commands::release::cut(&lab.work, &request, &ReleaseScheme::Dated);
    assert!(
        outcome.is_err(),
        "a parent-count mismatch must refuse, got {outcome:?}"
    );

    // And the dated name was not set on a bad cut. Asked of the bookmark list,
    // because resolving a bookmark that rightly does not exist is an error.
    let names = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !names
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-07-30"),
        "a refused cut still claimed the name"
    );
}

#[test]
fn fixed_previous_position_keeps_the_published_remote_after_a_local_cut() {
    // Given: a fixed integration cut published to origin, then advanced only locally.
    let lab = lab::Lab::new();
    lab.branch("integration", "integration.txt", "published\n");
    lab.push_branch("integration");
    let published = Repo::open(&lab.work)
        .expect("open published repo")
        .resolve_commit("integration@origin")
        .expect("published integration tip");
    lab.jj_work(["new", "-r", "integration", "-m", "local integration cut"]);
    std::fs::write(lab.work.join("integration.txt"), "local cut\n").expect("write local cut");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: Some("integration".to_owned()),
        test_count_command: None,
        consumers: Vec::new(),
    };
    let repo = Repo::open(&lab.work).expect("open after local cut");
    let local = repo
        .resolve_commit("integration")
        .expect("local integration tip");

    // When: the previous fixed release position is read after the local cut.
    let previous = knives::commands::release::previous_position(&repo, &entry);

    // Then: it is the unchanged published remote, not the new local cut.
    assert_ne!(local, published);
    assert_eq!(previous, Some(("integration@origin".to_owned(), published)));
}

#[test]
fn a_fixed_release_branch_is_cut_in_place_and_its_previous_position_is_the_old_cut() {
    // Given: a fork with one feature branch and a fixed integration branch scheme.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration");
    let scheme = entry.release_scheme();

    // When: the first fixed cut is made and pushed.
    let opened = Repo::open(lab.work_path()).expect("open");
    let carried = knives::release_model::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches");
    let trunk = opened
        .resolve_commit(&entry.upstream_trunk())
        .expect("upstream trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    let first = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("first cut");
    lab.push_branch("integration");
    lab.fetch_work();

    // MANDATORY reopen: Repo::open reads state at call time, and the first handle
    // predates the push/fetch that made integration@origin available locally.
    let opened = Repo::open(lab.work_path()).expect("reopen after fetch");
    let previous = knives::commands::release::previous_position(&opened, &entry)
        .expect("a pushed cut is a previous position");

    // Then: the remote-tracking ref is the old cut before any subsequent push.
    assert_eq!(
        previous,
        ("integration@origin".to_owned(), first.clone()),
        "the old cut is the previous release"
    );

    lab.branch("feat/beta", "beta.txt", "two\n");
    let opened = Repo::open(lab.work_path()).expect("reopen for second cut");
    let carried = knives::release_model::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches for second cut");
    let trunk = opened
        .resolve_commit(&entry.upstream_trunk())
        .expect("upstream trunk for second cut");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    let second = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("second fixed cut may move integration sideways");
    let opened = Repo::open(lab.work_path()).expect("reopen after second cut");

    assert_eq!(
        opened
            .resolve_commit("integration")
            .expect("integration tip"),
        second,
        "the fixed bookmark advances to the fresh flat merge"
    );
    assert_eq!(
        knives::commands::release::previous_position(&opened, &entry),
        Some(("integration@origin".to_owned(), first)),
        "the still-unpushed second cut keeps the first published cut as previous"
    );
}

#[test]
fn a_dated_cut_refuses_a_sideways_bookmark_move() {
    // Given: two unrelated flat dated cuts.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    lab.branch("feat/beta", "beta.txt", "two\n");
    lab.octopus("release/2026-08-01", "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "three\n");
    lab.octopus("release/2026-08-02", "feat/alpha", "feat/gamma");
    let replacement = Repo::open(lab.work_path())
        .expect("open")
        .parents_of("release/2026-08-02")
        .expect("replacement dated cut parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect();

    // When: cut rebuilds the second merge under the first dated name.
    let moved = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "release/2026-08-01".to_owned(),
            parents: replacement,
            provenance: vec![],
        },
        &ReleaseScheme::Dated,
    );

    // Then: Dated routing retains jj's sideways-move protection.
    assert!(moved.is_err(), "dated cuts must not move sideways");
}

#[test]
fn plan_for_a_fixed_release_ignores_a_non_publish_remote() {
    // Given: the same fixed release exists on both the publish remote and upstream.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration");
    let scheme = entry.release_scheme();
    let opened = Repo::open(lab.work_path()).expect("open");
    let carried = knives::release_model::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches");
    let trunk = opened
        .resolve_commit(&entry.upstream_trunk())
        .expect("upstream trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("cut");
    lab.push_branch("integration");
    lab.jj_work(["bookmark", "track", "integration", "--remote", "upstream"]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "upstream",
        "--bookmark",
        "integration",
    ]);
    lab.fetch_work();
    let upstream = Repo::open(lab.work_path())
        .expect("reopen after fetch")
        .resolve_commit("integration@upstream");
    assert!(upstream.is_ok(), "upstream fixed release must be present");
    lab.jj_work(["bookmark", "delete", "integration"]);

    // When: planning selects the newest fixed release without a local bookmark.
    let plan = knives::commands::release::plan(&knives::ids::RepoName::new("a-repo"), &entry, &[])
        .expect("plan");

    // Then: upstream cannot be mistaken for the publish remote's release.
    assert_eq!(plan.release.as_deref(), Some("integration@origin"));
}

#[test]
fn parallel_landed_probes_answer_exactly_what_serial_ones_did() {
    // The comment on `maintained_branches` already said these were most of the
    // runtime. Parallelising them may not change one reported fact, so the proof
    // is the two reports rendering identically.
    let lab = lab::Lab::new();
    for index in 0..6 {
        lab.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    lab.publish_pull("feat/b0", 1);
    lab.squash_merge_pull(1, None);
    lab.fetch_work();
    let entry = lab_entry(&lab);
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let options = |workers: usize| knives::commands::status::Options {
        probe: true,
        forge: None,
        cache: None,
        registry: None,
        ledger: None,
        workers,
    };
    let name = knives::ids::RepoName::new("demo");

    // When: the same repository is gathered serially and on several threads
    let serial = status::gather(&name, &entry, &store, &options(1)).expect("serial gather");
    let parallel = status::gather(&name, &entry, &store, &options(8)).expect("parallel gather");

    // Then: not one token differs, including the landed column
    assert_eq!(
        status::render::render(&serial, true),
        status::render::render(&parallel, true),
        "parallelism changed the report"
    );
    assert_eq!(status::exit_for(&serial), status::exit_for(&parallel));
    assert_eq!(
        parallel
            .branches
            .iter()
            .map(|row| row.landed)
            .collect::<Vec<_>>(),
        serial
            .branches
            .iter()
            .map(|row| row.landed)
            .collect::<Vec<_>>()
    );
    assert!(
        parallel
            .branches
            .iter()
            .any(|row| row.landed == Some(knives::detect::LandedVerdict::InTrunk)),
        "the merged branch must still be judged in-trunk: {parallel:?}"
    );
}
#[test]
fn status_all_reports_every_repo_in_registry_order() {
    // Given: two managed forks in one registry, named so registry order and
    // completion order can differ — the small one finishes first and must still
    // print second.
    let first = Lab::new();
    first.branch("feat/alpha", "alpha.txt", "alpha\n");
    let second = Lab::new();
    for index in 0..6 {
        second.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.aardvark]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/one.git\"\n\
             [repos.zebra]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/two.git\"\n",
            second.work.display(),
            second.upstream.display(),
            first.work.display(),
            first.upstream.display(),
        ),
    )
    .expect("write registry");

    // When: every repo is reported at once, from outside both of them.
    let elsewhere = tempfile::tempdir().expect("somewhere else");
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "--all", "--no-github", "--no-landed"])
        .current_dir(elsewhere.path())
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_TIMING", "1")
        .output()
        .expect("run status --all");

    // Then: both are present, in registry order, whichever finished first.
    let text = String::from_utf8_lossy(&output.stdout);
    let aardvark = text.find("aardvark").expect("first repo reported");
    let zebra = text.find("zebra").expect("second repo reported");
    assert!(
        aardvark < zebra,
        "repos were rendered out of registry order: {text}"
    );
    assert!(text.contains("feat/b5"), "was: {text}");
    assert!(text.contains("feat/alpha"), "was: {text}");
    // And: the timing lines go to stderr, so a script's stdout is still a report.
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("timing aardvark:"), "was: {errors}");
    assert!(errors.contains("timing zebra:"), "was: {errors}");
    assert!(
        !text.contains("timing "),
        "timings leaked into stdout: {text}"
    );

    // And: `--all` is exactly each repository's own report, in registry order,
    // joined the way the serial loop joined them. `--no-landed` makes it exact:
    // with no probes, a single repo's larger probe budget cannot change a token.
    let alone = |repo: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(["--text", "status", repo, "--no-github", "--no-landed"])
            .current_dir(elsewhere.path())
            .env("KNIVES_CONFIG_HOME", home.path())
            .output()
            .expect("run status for one repo");
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    };
    assert_eq!(
        text.trim_end(),
        format!("{}\n\n{}", alone("aardvark"), alone("zebra")),
        "--all is not each repo's own report in registry order"
    );
}

#[test]
fn status_and_plan_report_a_double_cut() {
    // Given: a published cut, then a local release name moved sideways to a
    // sibling with different content. Both the local and origin names are in
    // the release trust boundary.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.push_branch("release/2026-08-04");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    knives::jj::set_bookmark_anywhere(&lab.work, "release/2026-08-04", "feat/beta")
        .expect("move local release sideways");
    lab.jj_work(["workspace", "update-stale"]);

    // When: status and the bare release command observe the two named cuts.
    let status = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-github", "--no-landed"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run status");
    let plan = knives_release(&lab, &home, &[]);

    // Then: both read the changed trees as the same release name cut twice.
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert_eq!(
        status.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "status missed the double cut: {status_text}\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status_text.contains("double-cut") && status_text.contains("release/2026-08-04"),
        "status did not name the double cut: {status_text}"
    );
    let plan_text = String::from_utf8_lossy(&plan.stdout);
    assert_eq!(
        plan.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "plan missed the double cut: {plan_text}\n{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        plan_text.contains("release/2026-08-04 names both")
            && plan_text.contains("their trees differ"),
        "plan did not carry the double-cut finding: {plan_text}"
    );

    // Given: another published cut whose local name is rebuilt with `jj
    // duplicate`, retaining the release tree while getting a different commit.
    let rebuilt = Lab::new();
    rebuilt.branch("feat/alpha", "alpha.txt", "alpha\n");
    rebuilt.branch("feat/beta", "beta.txt", "beta\n");
    let (rebuilt_home, _consumer) = home_after_first_cut(&rebuilt);
    rebuilt.push_branch("release/2026-08-04");
    let published = commit_at(&rebuilt, "release/2026-08-04");
    rebuilt.jj_work(["duplicate", "release/2026-08-04"]);
    let duplicated = knives::jj::commits_matching(&rebuilt.work, "all()")
        .expect("list duplicated release candidates")
        .into_iter()
        .find(|candidate| {
            candidate != &published
                && knives::jj::changed_files_between(
                    &rebuilt.work,
                    published.as_str(),
                    candidate.as_str(),
                )
                .expect("compare duplicate tree")
                .is_empty()
        })
        .expect("find the duplicated release");
    knives::jj::set_bookmark_anywhere(&rebuilt.work, "release/2026-08-04", duplicated.as_str())
        .expect("point local release at duplicated cut");
    rebuilt.jj_work(["workspace", "update-stale"]);

    // When: the same two commands compare the rebuilt cut to its published copy.
    let rebuilt_status = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-github", "--no-landed"])
        .current_dir(&rebuilt.work)
        .env("KNIVES_CONFIG_HOME", rebuilt_home.path())
        .output()
        .expect("run rebuilt status");
    let rebuilt_plan = knives_release(&rebuilt, &rebuilt_home, &[]);

    // Then: identical trees are a note, not the changed-content finding.
    let rebuilt_status_text = String::from_utf8_lossy(&rebuilt_status.stdout);
    assert!(
        rebuilt_status.status.success(),
        "status treated an identical rebuild as a finding: {rebuilt_status_text}\n{}",
        String::from_utf8_lossy(&rebuilt_status.stderr)
    );
    assert!(
        rebuilt_status_text
            .contains("release/2026-08-04 names two commits with identical trees (a rebuilt cut)"),
        "status omitted the rebuilt-cut note: {rebuilt_status_text}"
    );
    let rebuilt_plan_text = String::from_utf8_lossy(&rebuilt_plan.stdout);
    assert!(
        rebuilt_plan.status.success(),
        "plan treated an identical rebuild as a finding: {rebuilt_plan_text}\n{}",
        String::from_utf8_lossy(&rebuilt_plan.stderr)
    );
    assert!(
        rebuilt_plan_text
            .contains("release/2026-08-04 names two commits with identical trees (a rebuilt cut)"),
        "plan omitted the rebuilt-cut note: {rebuilt_plan_text}"
    );
}

#[test]
fn status_with_the_landed_probe_reports_a_merged_branch_and_leaves_no_trace() {
    // The probe path through `knives status` end to end. It is exercised here and
    // deliberately never against a live shared repository, because it mutates.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);

    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    let before = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: true,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");
    let after = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");

    // The merged branch reads as already in the trunk. Stated on the branch itself
    // rather than as a finding: it is a fact about the branch, not something wrong.
    let verdicts: Vec<_> = report
        .branches
        .iter()
        .filter_map(|row| row.landed)
        .collect();
    assert!(
        verdicts.contains(&knives::detect::landed::LandedVerdict::InTrunk),
        "expected the squash-merged branch to read as in-trunk: {verdicts:?}"
    );

    // And the probe left the repository as it found it.
    assert_eq!(before, after, "the probe left commits behind");
}

#[test]
fn status_reports_empty_diff_and_deleted_head_from_completed_facts() {
    // Given: an open branch pull whose completed fact row reports no diff and no head ref
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let pulls = format!(
        "[{}]",
        pull_record_with_fields(
            7,
            "OPEN",
            "feat/alpha",
            r#","additions":0,"deletions":0,"changedFiles":0,"headRef":null"#,
        )
    );
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh(shim.path(), &pulls, &[], None);

    // When: the real status binary consumes the completed snapshot
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-landed"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run status with a forge shim");

    // Then: both answered incidents are visible and findings determine the exit code
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("empty-diff"), "stdout: {stdout}");
    assert!(stdout.contains("deleted-head-ref"), "stdout: {stdout}");
}

#[test]
fn status_reports_branch_overlap_after_upstream_advances_without_landed_probe() {
    // Given: two maintained branches from a trunk revision that upstream has since advanced past
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.advance_upstream("upstream advanced past the branches\n");
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let store_path = lab
        .work
        .parent()
        .expect("lab work directory has a parent")
        .join("state.json");
    let store = knives::store::Store::open(store_path).expect("store");
    let name = knives::ids::RepoName::new("a-repo");

    // When: status deliberately skips only the landed replay
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the independent path comparison still reports the shared file
    let overlap = report
        .findings
        .iter()
        .find(|finding| {
            finding.kind == knives::detect::FindingKind::BranchOverlap
                && finding.subject == knives::detect::Subject::File("shared.txt".to_owned())
        })
        .expect("the shared file is reported even without the landed probe");
    assert!(overlap.detail.contains("feat/alpha"), "was: {overlap:?}");
    assert!(overlap.detail.contains("feat/beta"), "was: {overlap:?}");
}

#[test]
fn status_reports_a_branch_carried_elsewhere() {
    // Given: an open branch whose tip is an ancestor of another local bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status gathers the branch report
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the branch fact names the reference that reaches its tip
    assert!(report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
            && finding.detail.contains("theirs/rework")
    }));
}

#[test]
fn status_reports_a_carrier_for_a_closed_pull_request() {
    // Given: a closed pull request whose branch is reachable from another bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");
    let forge = knives::forge::fake::FakeForge {
        pull_requests: std::iter::once((
            BranchName::new("feat/alpha"),
            pulls::pull_request(7, "CLOSED", "feat/alpha"),
        ))
        .collect(),
        ..knives::forge::fake::FakeForge::default()
    };

    // When: status gathers the branch report with the closed pull request
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: forge state does not suppress the local ancestry fact
    assert!(report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
            && finding.detail.contains("theirs/rework")
    }));
}

#[test]
fn status_does_not_report_trunk_as_a_carrier_without_landed_probe() {
    // Given: a branch whose tip is reachable from the local trunk bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "feat/alpha",
        "-m",
        "trunk carries feature",
    ]);
    lab.jj_work(["bookmark", "set", "main", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status skips the landed probe
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: trunk is never a carrier finding, even without an InTrunk verdict
    assert!(!report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
    }));
}

#[test]
fn status_carries_each_branchs_newest_notch_in_json_and_in_text() {
    // Given: a fork with a note followed by a newer machine event on one branch.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work,
        "ses_fff688".to_owned(),
    );
    scribe
        .record(&knives::ledger::Draft {
            subject: Some("feat/alpha"),
            kind: knives::ledger::Kind::Note,
            disposition: None,
            text: "human conclusion".to_owned(),
            evidence: Vec::new(),
            pr: None,
        })
        .expect("human note");
    scribe
        .event(
            Some("feat/alpha"),
            "claim released; superseded by feat/next".to_owned(),
            None,
        )
        .expect("machine event");
    scribe
        .event(
            Some("feat/unrelated"),
            "claimed: something else".to_owned(),
            None,
        )
        .expect("other branch");

    // When: status gathers with the ledger available
    let report = status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: Some(&ledger),
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the human note wins over the newer event, while the row still says
    // how many ledger entries it condensed.
    let alpha = report
        .branches
        .iter()
        .find(|row| row.name.as_str() == "feat/alpha")
        .expect("the branch has a row");
    let last = alpha.last_notch.as_ref().expect("a breadcrumb");
    assert_eq!(last.text, "human conclusion");
    assert_eq!(last.kind, knives::ledger::Kind::Note);
    assert_eq!(last.count, 2);

    // And: it survives serialisation under the name the design fixed
    let json = serde_json::to_value(&report).expect("report serialises");
    let rows = json["branches"].as_array().expect("branches");
    let row = rows
        .iter()
        .find(|row| row["name"] == "feat/alpha")
        .expect("row");
    assert_eq!(row["last_notch"]["kind"], "note");
    assert_eq!(row["last_notch"]["text"], "human conclusion");
    assert_eq!(row["last_notch"]["count"], 2);
    assert!(row["last_notch"]["ts"].is_string());
    let beta = rows
        .iter()
        .find(|row| row["name"] == "feat/beta")
        .expect("branch without a notch");
    assert!(
        beta.get("last_notch").is_none(),
        "no notch is absent, not null: {beta}"
    );

    // And: the branch line carries one token for it and its masked sibling
    let text = status::render::render(&report, false);
    assert!(text.contains("\"human conclusion\" (now)+1"), "was: {text}");
    assert!(report.repo_notches.is_none(), "was: {report:?}");
    assert!(
        json.get("repo_notches").is_none(),
        "absent when no repo-level entry: {json}"
    );
    assert!(
        !text.contains("repo-level"),
        "no repo-level summary without a repo-level entry: {text}"
    );
}

#[test]
fn status_carries_repo_level_notches_in_json_and_text() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = sync_entry(&lab);
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work,
        "ses_fff688".to_owned(),
    );
    scribe
        .event(None, "release remote needs a refresh".to_owned(), None)
        .expect("repo-level notch");

    let report = status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: Some(&ledger),
            workers: 1,
        },
    )
    .expect("gather");

    let json = serde_json::to_value(&report).expect("report serialises");
    assert_eq!(json["repo_notches"]["count"], 1);
    assert_eq!(json["repo_notches"]["last"]["kind"], "event");
    assert_eq!(
        json["repo_notches"]["last"]["text"],
        "release remote needs a refresh"
    );
    let text = status::render::render(&report, false);
    assert!(
        text.contains("notches  1 repo-level, newest: \"release remote needs a refresh\""),
        "was: {text}"
    );
}

#[test]
fn a_fresh_cut_carries_every_branch_and_nothing_else() {
    // What a dated release is: a flat merge of the current tip of everything we
    // carry. Not the trunk, not other releases.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-07-29", "feat/alpha", "feat/beta");

    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let carried = knives::release_model::carried_branches(&repo, "main", &ReleaseScheme::Dated)
        .expect("carried");
    let names: Vec<&str> = carried.iter().map(|(branch, _)| branch.as_str()).collect();

    assert!(names.contains(&"feat/alpha"));
    assert!(names.contains(&"feat/beta"));
    assert!(
        !names.iter().any(|n| n.starts_with("release/")),
        "a release is not a branch we carry"
    );
    assert!(
        !names.contains(&"main"),
        "the trunk is not a branch we carry"
    );
}

#[test]
fn cutting_a_release_reaps_the_superseded_one() {
    // Given: an existing cut and a consumer following it by branch.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: a newer release is cut through the binary.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the superseded cut is reaped while the newer one remains.
    assert!(
        output.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    let tips = Repo::open(&lab.work)
        .expect("reopen release repository")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04")
    );
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );
}

#[test]
fn a_named_cut_with_an_inconclusive_content_audit_returns_findings() {
    // Given: two members entangled in one file, so the cut is conflicted from birth
    // and every member's replay onto it answers nothing either way.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: the cut is named despite its deliberately non-fatal audit result.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is retained, but automation receives the unresolved finding.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("content check inconclusive"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Repo::open(&lab.work)
            .expect("open named cut")
            .resolve_commit("release/2026-08-05")
            .is_ok(),
        "the inconclusive cut was not named"
    );
}

#[test]
fn a_named_cut_that_drops_the_test_count_returns_findings() {
    // Given: one member reports ten tests while another makes the merged tree
    // report five. The check compares the cut against a single contributing
    // branch — the first parent — so the low-count member must not be first.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "branch-count", "5\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\ntest_count_command = \"if test -f branch-count; then cat branch-count; else printf 10; fi\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("configure test counter");

    // When: the real cut command observes the lower count in its new tree.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut remains named but reports the dropped-suite finding to automation.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dropped that branch's tests"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_rebase_refuses_when_every_pin_is_frozen() {
    // Given: a dated release whose only consumer pins it by revision. Moving the
    // bookmark in place would reach nobody, so this requires a new dated cut.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write registry");
    lab.advance_upstream("upstream advance\n");
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("resolve release before refusal");

    // When: the real binary is asked to rebase the release onto upstream.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it directs the caller to a dated cut, exits incomplete, and does not move it.
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "frozen-pin guidance missing: {stdout}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release after refusal");
    assert_eq!(before, after, "a frozen release was moved in place");
}

#[test]
fn release_rebase_refusal_for_fixed_release_explains_that_revision_pins_cannot_follow_it() {
    // Given: a fixed release branch whose only consumer pin is a frozen revision.
    let lab = Lab::new();
    let release = "integration";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "# checkout pin\nwork = { git = \"https://forge.invalid/acme/work.git\", rev = \"integration\" }\n",
        "# origin pin\nwork = { git = \"https://forge.invalid/acme/work.git\", rev = \"integration\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write fixed-release registry");
    lab.advance_upstream("upstream advance\n");

    // When: the fixed release is asked to move in place.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it is incomplete and names the only viable remediation.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("update the frozen consumer pins")
            && stdout.contains("fixed branches cannot reach revision pins"),
        "fixed-scheme guidance missing: {stdout}"
    );
}

#[test]
fn release_rebase_repairs_a_followed_dated_release_with_a_sideways_merge() {
    // Given: an existing dated release, a consumer that follows it, and a new upstream commit.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let previous_parents = Repo::open(&lab.work)
        .expect("open release repository")
        .parents_of(release)
        .expect("read existing release parents");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let upstream = Repo::open(&lab.work)
        .expect("reopen release repository")
        .resolve_commit("main@upstream")
        .expect("resolve advanced upstream");
    // When: the repair command moves the existing release onto a new flat merge.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the command succeeds; the legacy trunk parent is shed — the base is
    // never a parent — and the members were rebased onto the new upstream,
    // their bookmarks following.
    assert!(
        output.status.success(),
        "release rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repo = Repo::open(&lab.work).expect("reopen repaired release repository");
    let parents = release_parent_commits(&lab, release);
    let old_base = &previous_parents[0]; // lab.octopus puts main@origin first
    assert!(
        !parents.contains(&old_base.commit),
        "superseded base still a parent: {parents:?}"
    );
    assert!(
        !parents.contains(&upstream),
        "the new upstream must not become a parent either: {parents:?}"
    );
    assert_eq!(
        parents,
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "the rewritten members are the whole parent set"
    );
    assert!(
        repo.is_ancestor(&upstream, &commit_at(&lab, release))
            .expect("ancestry"),
        "the release must contain the upstream through its members"
    );
}

#[test]
fn a_second_rebase_does_not_grow_the_release() {
    // Given: a release rebased once already, and upstream advancing again.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is rebased after each upstream advance.
    for advance in ["first advance\n", "second advance\n"] {
        lab.advance_upstream(advance);
        let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);
        assert!(
            output.status.success(),
            "rebase failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Then: two parents — the members — whatever the rebase count. The first
    // rebase sheds the legacy trunk parent and no rebase adds one back.
    let parents = Repo::open(&lab.work)
        .expect("open")
        .parents_of(release)
        .expect("parents");
    assert_eq!(parents.len(), 2, "parents accumulated: {parents:?}");
}

#[test]
fn a_rebase_refuses_a_stale_parent_it_cannot_map() {
    // Given: a standard release whose captured alpha tip is no longer held by alpha.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let stale_parent = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve captured alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["new", "-r", "feat/alpha"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release before refusal");

    // When: the real binary attempts to replace the release base.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it refuses rather than carrying the stale parent or moving the release.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains(&stale_parent.as_str()[..12]),
        "refusal must name the stale parent: {text}"
    );
    assert!(
        text.contains("feat/alpha (now "),
        "refusal must say where alpha moved: {text}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release after refusal");
    assert_eq!(before, after, "refusal moved the release");
}

#[test]
fn a_rebase_keeps_a_landed_bookmark_held_member() {
    // Given: alpha is both held by its bookmark and reachable from the explicit replacement.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let alpha = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-m",
        "upstream merge carrying alpha",
    ]);
    let replacement = lab.revision(&lab.work, "@", "commit_id");
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: rebase uses that merge as its explicit replacement base.
    let output = knives_release(&lab, &home, &["rebase", replacement.trim()]);

    // Then: alpha remains a direct member parent despite being reachable from replacement.
    assert!(
        output.status.success(),
        "rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = Repo::open(&lab.work)
        .expect("reopen")
        .parents_of(release)
        .expect("read repaired parents");
    assert_eq!(parents.len(), 2, "parents: {parents:?}");
    assert!(
        parents.iter().any(|parent| parent.commit == alpha),
        "landed alpha was dropped: {parents:?}"
    );
}

#[test]
fn a_rebase_moves_the_whole_composition_onto_the_target() {
    // Given: two members forked from the old trunk, their merge conflict
    // resolved on the release, and an upstream that has advanced. `rebase` is
    // `jj rebase -b <release> -d <target>`: the members move onto the target
    // and the release moves with them — the trunk never becomes a parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first cut")
            .resolve_commit("release/2026-08-04")
            .is_ok(),
        "first cut was not named: {first:?}"
    );
    lab.jj_work(["new", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "resolved\n").expect("resolve conflict");
    lab.jj_work(["squash"]);
    let old_alpha = commit_at(&lab, "feat/alpha");
    lab.advance_upstream("advance\n");

    // When: the composition is rebased onto the advanced trunk.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);
    assert!(
        output.status.success(),
        "rebase failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the members were rewritten onto the new trunk and their bookmarks
    // followed; the release's parents are exactly the moved members.
    let repo = Repo::open(&lab.work).expect("reopen after rebase");
    let trunk = commit_at(&lab, "main@upstream");
    let new_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(new_alpha, old_alpha, "alpha was not rewritten");
    assert!(
        repo.is_ancestor(&trunk, &new_alpha).expect("ancestry"),
        "alpha does not sit on the new trunk"
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(parents.contains(&new_alpha), "{parents:?}");
    assert!(
        !parents.contains(&trunk),
        "the trunk must not become a parent: {parents:?}"
    );
    assert_eq!(parents.len(), 2, "{parents:?}");
    assert!(
        repo.is_ancestor(&trunk, &commit_at(&lab, "release/2026-08-04"))
            .expect("ancestry"),
        "the release does not contain the new trunk through its members"
    );
    // And: the recorded resolution replayed through the rebase.
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "shared.txt"),
        "resolved\n"
    );
}

/// Run the knives binary's release command against the complete snapshot forge
/// protocol, with an isolated cache root.
fn knives_release_with_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    pulls: &str,
    args: &[&str],
) -> std::process::Output {
    knives_release_with_forge_withheld_facts(ReleaseWithSnapshotForgeInput {
        lab,
        home,
        pulls,
        withheld_facts: &[],
        args,
    })
}

#[derive(Clone, Copy)]
struct ReleaseWithSnapshotForgeInput<'a> {
    lab: &'a Lab,
    home: &'a tempfile::TempDir,
    pulls: &'a str,
    withheld_facts: &'a [u64],
    args: &'a [&'a str],
}

fn knives_release_with_forge_withheld_facts(
    input: ReleaseWithSnapshotForgeInput<'_>,
) -> std::process::Output {
    release_with_snapshot_forge(input, ReleaseOutput::Text)
}

fn release_with_snapshot_forge(
    input: ReleaseWithSnapshotForgeInput<'_>,
    output: ReleaseOutput,
) -> std::process::Output {
    let ReleaseWithSnapshotForgeInput {
        lab,
        home,
        pulls,
        withheld_facts,
        args,
    } = input;
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh(shim.path(), pulls, withheld_facts, None);
    release_command(lab, home, output, args)
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run knives release with a forge shim")
}

#[test]
fn a_bare_rebase_with_no_merged_pull_request_requires_a_commit() {
    // Given: a release in hand, and a forge whose only pull request is still open.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let before = commit_at(&lab, release);
    let pulls = format!("[{}]", pull_record(7, "OPEN", "feat/alpha", None));

    // When: the bare rebase asks the forge for a default target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: with nothing merged there is no default, and the release stays put.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: no pull request has merged, so there is no default target; \
             provide a commit to rebase onto"
        ),
        "missing refusal guidance: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_reads_merged_pulls_through_the_snapshot() {
    // Given: alpha merged upstream by a merge commit, beta still open, and the
    // trunk advanced past that merge afterwards.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond the merge\n");
    let tip = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str())),
        pull_record(8, "OPEN", "feat/beta", None)
    );

    // When: the bare rebase asks the forge instead of taking a target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the release lands on the merge commit, not the later tip.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: every merged pull request (#7) is in main@upstream by {}; rebasing onto it",
            &merged_at.as_str()[..12]
        )),
        "missing target explanation: {stdout}"
    );
    let repo = Repo::open(&lab.work).expect("reopen after the default rebase");
    let at_release = commit_at(&lab, release);
    assert!(
        repo.is_ancestor(&merged_at, &at_release).expect("ancestry"),
        "the release does not contain the merge commit"
    );
    assert!(
        !repo.is_ancestor(&tip, &at_release).expect("ancestry"),
        "the release overshot the merged point onto the trunk tip"
    );
}

#[test]
fn a_bare_rebase_covers_the_latest_of_several_merged_pull_requests() {
    // Given: alpha and gamma merged upstream in that order, so gamma's merge
    // commit is the first trunk commit containing both.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/gamma", 8);
    lab.merge_pull_with_merge_commit(8);
    let second_merge = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond both merges\n");
    let tip = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/gamma", Some(second_merge.as_str()))
    );

    // When: the bare rebase chooses among several merged pull requests.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the later merge commit wins because it contains the earlier one.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: every merged pull request (#7, #8) is in main@upstream by {}; \
             rebasing onto it",
            &second_merge.as_str()[..12]
        )),
        "missing target explanation: {stdout}"
    );
    let repo = Repo::open(&lab.work).expect("reopen after the default rebase");
    let at_release = commit_at(&lab, release);
    assert!(
        repo.is_ancestor(&second_merge, &at_release)
            .expect("ancestry"),
        "the release does not contain the covering merge commit"
    );
    assert!(
        !repo.is_ancestor(&tip, &at_release).expect("ancestry"),
        "the release overshot the merged point onto the trunk tip"
    );
}

#[test]
fn a_bare_rebase_refuses_when_a_merged_candidate_fact_is_omitted() {
    // Given: discovery names two merged pull requests that are on local trunk,
    // but the facts batch answers only the latter one.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/gamma", 8);
    lab.merge_pull_with_merge_commit(8);
    let second_merge = commit_at(&lab, "main@upstream");
    let before = commit_at(&lab, release);
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/gamma", Some(second_merge.as_str()))
    );

    let output = knives_release_with_forge_withheld_facts(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[7],
        args: &["rebase"],
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let text = format!("{stdout}\n{stderr}");
    assert!(
        text.contains("#7") && !text.contains("rebasing onto it"),
        "the refusal did not name the unanswered merged pull request: {text}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_refuses_merged_work_missing_from_the_local_trunk() {
    // Given: the forge says a pull request merged, but its merge commit is not
    // in the local repository at all.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let before = commit_at(&lab, release);
    let pulls = format!(
        "[{}]",
        pull_record(
            7,
            "MERGED",
            "feat/alpha",
            Some("feedfacefeedfacefeedfacefeedfacefeedface")
        )
    );

    // When: the bare rebase tries to place that merge on the local trunk.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: it refuses with fetch guidance rather than guessing, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: the merge commit(s) of #7 are not in the local main@upstream; \
             run knives sync, or provide a commit to rebase onto"
        ),
        "missing fetch guidance: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_drops_a_member_whose_pull_request_landed() {
    // Given: alpha merged upstream by a merge commit and beta still open.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond the merge\n");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str())),
        pull_record(8, "OPEN", "feat/beta", None)
    );

    // When: the bare rebase lands on the merge commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha's parent is dropped, its bookmark untouched, and the drop is
    // recorded on the release itself.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dropped feat/alpha: landed upstream as #7"),
        "missing drop report: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release),
        vec![commit_at(&lab, "feat/beta")],
        "the landed member must be the only parent removed"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen")
            .resolve_commit("feat/alpha")
            .is_ok(),
        "dropping the parent must not touch the branch"
    );
    assert!(
        lab.revision(&lab.work, release, "description")
            .contains("dropped feat/alpha: landed upstream as #7"),
        "the release description must record the drop"
    );
}

#[test]
fn no_drop_keeps_a_landed_member_as_a_parent() {
    // Given: the same landed alpha, and the caller asking to keep it.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    let alpha = commit_at(&lab, "feat/alpha");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase runs with --no-drop.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase", "--no-drop"]);

    // Then: the landed member stays a parent and nothing reports a drop.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stdout.contains("dropped feat/alpha"), "{stdout}");
    assert!(
        release_parent_commits(&lab, release).contains(&alpha),
        "--no-drop must keep the landed member"
    );
}

#[test]
fn a_bare_rebase_drops_a_member_landed_by_squash() {
    // Given: alpha squash-merged, so its commits replay empty onto the target
    // without the tip ever entering the trunk's history.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase lands on the squash commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha is dropped and beta's rewritten tip is the only parent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dropped feat/alpha: landed upstream as #7"),
        "missing drop report: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release),
        vec![commit_at(&lab, "feat/beta")],
        "the squash-landed member must be dropped"
    );
}

#[test]
fn a_bare_rebase_keeps_a_landed_branch_that_carries_work_past_its_pull() {
    // Given: alpha's pull request holds only its first commit, the branch has a
    // second commit past it, and the release carries the extended tip. The pull
    // then squash-merges, so the branch still holds work the trunk lacks.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    extend_branch(&lab, "feat/alpha", "alpha-more.txt", "work past the pull\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase lands on the squash commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha is kept, with the reason stated, and both members remain.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("kept feat/alpha: it carries work past #7"),
        "missing keep report: {stdout}"
    );
    assert!(!stdout.contains("dropped feat/alpha"), "{stdout}");
    assert_eq!(
        release_parent_commits(&lab, release).len(),
        2,
        "both members must remain parents"
    );
}

#[test]
fn a_rebase_refuses_a_composition_whose_every_member_landed() {
    // Given: a single-member release whose only branch merged upstream. jj
    // would re-parent the release straight onto the destination, leaving the
    // trunk as its only parent — and the base is never a parent.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", release]);
    assert!(cut.status.success(), "{cut:?}");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let before = commit_at(&lab, release);

    // When: the rebase is pointed past the landing, explicitly.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it refuses rather than making the trunk a parent, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "every member of release/2026-08-04 has landed in main@upstream; rebasing would \
             make the trunk the only parent, so nothing moved \u{2014} reap the release or \
             include new work"
        ),
        "missing fully-landed refusal: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_release_already_at_its_target_still_drops_landed_members() {
    // Given: both members squash-merged and the release already rebased onto
    // their covering commit with --no-drop, so it carries two landed members
    // as empty replayed chains.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/beta", 8);
    lab.squash_merge_pull(8, None);
    let second_merge = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/beta", Some(second_merge.as_str()))
    );
    let kept = knives_release_with_forge(&lab, &home, &pulls, &["rebase", "--no-drop"]);
    assert!(kept.status.success(), "{kept:?}");

    // When: the bare rebase finds the release already contains the target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: nothing moves, and dropping every landed member is refused because
    // it would leave the release without a parent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("already contains"),
        "missing already-contains skip: {stdout}"
    );
    assert!(
        stdout.contains(
            "every member of release/2026-08-04 landed; dropping them all would leave it \
             without a parent, so nothing was dropped"
        ),
        "missing parentless-drop refusal: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release).len(),
        2,
        "a refused drop must leave the parents alone"
    );
}

#[test]
fn a_release_already_containing_the_reference_by_ancestry_is_left_alone() {
    // Given: a release whose alpha parent merged the upstream advance, so the
    // seed trunk is reachable through alpha rather than a direct release parent.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let seed = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@upstream")
        .expect("seed tip");
    lab.advance_upstream("advance\n");
    lab.jj_work([
        "new",
        "feat/alpha",
        "main@upstream",
        "-m",
        "merge upstream into alpha",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.jj_work(["new", "-r", "feat/alpha", "-r", "feat/beta", "-m", release]);
    lab.jj_work(["bookmark", "create", release, "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release");

    // When: asked to include the seed, an ancestor of alpha's merged base.
    let output = knives_release(&lab, &home, &["rebase", seed.as_str()]);

    // Then: containment is recognized and the release does not move.
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("already contains"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let after = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release");
    assert_eq!(
        before, after,
        "release moved for an already-contained commit"
    );
}

#[test]
fn a_release_contains_the_trunk_through_a_parents_history_not_as_a_direct_parent() {
    // Given: a member branch that merged the advanced upstream, and a release
    // whose direct parents are the seed trunk, that branch, and another branch.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.advance_upstream("advance\n");
    lab.jj_work([
        "new",
        "feat/alpha",
        "main@upstream",
        "-m",
        "merge upstream into alpha",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.octopus(release, "feat/alpha", "feat/beta");

    // When/Then: the probe sees containment through ancestry.
    let repo = Repo::open(&lab.work).expect("open");
    assert_eq!(
        knives::commands::release::trunk_lag(&repo, Some(release), "main@upstream"),
        None,
        "trunk is contained through feat/alpha's merge; the probe must not report lag"
    );
}

#[test]
fn preflight_reports_main_when_a_repo_configures_dev_as_its_trunk() {
    // Given: an upstream whose trunk is dev while main is a local work branch.
    let lab = lab::Lab::new();
    lab.jj_work(["bookmark", "set", "dev", "-r", "main"]);
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: Some("dev".to_owned()),
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };

    // When: preflight collects locally maintained branches.
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");

    // Then: dev is the only excluded trunk; main remains work to report.
    assert!(
        states.iter().any(|state| state.branch == "main"),
        "a non-trunk main branch must be reported, got {states:#?}"
    );
    assert!(
        !states.iter().any(|state| state.branch == "dev"),
        "the configured trunk is not a branch we maintain, got {states:#?}"
    );
}

#[test]
fn preflight_treats_a_fixed_release_branch_as_a_release_not_a_branch() {
    // Given: a fixed release bookmark and ordinary feature work.
    let lab = lab::Lab::new();
    lab.jj_work(["bookmark", "set", "integration", "-r", "main"]);
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: Some("integration".to_owned()),
        test_count_command: None,
        consumers: Vec::new(),
    };

    // When: preflight collects locally maintained branches.
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");

    // Then: the fixed cut is excluded while feature work remains visible.
    assert!(
        !states.iter().any(|state| state.branch == "integration"),
        "a fixed release is not a branch to preflight, got {states:#?}"
    );
    assert!(
        states.iter().any(|state| state.branch == "feat/alpha"),
        "feature work must still be preflighted, got {states:#?}"
    );
}

#[test]
fn preflight_hides_a_divergent_configured_trunk_bookmark() {
    // Given: the configured trunk has independently rewritten local and origin tips.
    let lab = lab::Lab::new();
    lab.branch("dev", "dev.txt", "dev\n");
    lab.rewrite_in_both_clones("dev");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: Some("dev".to_owned()),
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };

    // When: preflight reads divergent bookmarks before regular branch tips.
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");

    // Then: the trunk is excluded even when it is divergent.
    assert!(
        !states
            .iter()
            .any(|state| state.branch == "dev" && state.divergent),
        "the trunk must not appear as divergent work, got {states:#?}"
    );
}

#[test]
fn preflight_flags_a_branch_whose_tip_is_divergent() {
    // Pins a bug that shipped silently: divergence findings carry a CHANGE id,
    // and comparing them against a branch tip's COMMIT id never matches, so
    // nothing was ever reported as divergent.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.rewrite_in_both_clones("feat/alpha");

    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");
    assert!(
        states.iter().any(|state| state.divergent),
        "a branch whose tip is divergent must be reported as divergent, got {states:#?}"
    );
}

#[test]
fn the_probe_never_abandons_a_commit_it_did_not_create() {
    // Reproduction of a data-loss defect found in review. The cleanup used to
    // identify its own commits by set difference over children(onto). A dirty
    // `@` that is a child of `onto` has its commit id rewritten by any
    // snapshotting command, so it appeared in that difference and was abandoned.
    // Three commits and two bookmarks of another agent's work were destroyed by
    // a single `knives status`. Cleanup now abandons only ids jj reported creating.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");

    // Another agent's work, sitting as a child of the trunk with a dirty tree.
    lab.jj_work(["new", "main", "-m", "SOMEONE ELSE MID-TASK"]);
    std::fs::write(lab.work.join("their-wip.txt"), "precious\n").expect("write");
    lab.jj_work(["bookmark", "create", "their-work", "-r", "@"]);
    let theirs = lab.revision(&lab.work, "their-work", "commit_id");

    let _ = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/alpha"),
        "main",
    );

    // Their commit, their bookmark and their file all survive.
    let still_there = lab.revision(&lab.work, "their-work", "commit_id");
    assert_eq!(
        still_there.trim(),
        theirs.trim(),
        "the probe abandoned another agent's commit"
    );
    assert!(
        lab.work.join("their-wip.txt").exists(),
        "the probe destroyed another agent's uncommitted file"
    );
}

#[test]
fn cutting_a_release_does_not_move_another_agents_working_copy() {
    // Reproduction of a defect found in review. `create_merge` used `jj new`,
    // which moves `@`, so cutting a release parked whoever was working in the
    // repo's default workspace on top of the release octopus with their
    // uncommitted edits pending against it. That is verbatim the accident
    // `knives start` exists to prevent, caused by `knives release`.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    lab.jj_work(["new", "main", "-m", "SOMEONE ELSE MID-TASK"]);
    std::fs::write(lab.work.join("their-wip.txt"), "in progress\n").expect("write");
    let before = lab.revision(&lab.work, "@", "change_id");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());
    let _ = knives::jj::write_release(
        &lab.work,
        &knives::jj::ReleaseWrite {
            source: None,
            parents: &[alpha, beta],
            message: Some("release: test"),
            bookmark: None,
            operation: "knives: cut release: test",
        },
    )
    .expect("merge");

    let after = lab.revision(&lab.work, "@", "change_id");
    assert_eq!(
        before, after,
        "cutting a release moved someone else's working copy"
    );
    assert!(
        lab.work.join("their-wip.txt").exists(),
        "their uncommitted file vanished"
    );
}

#[test]
fn a_stranded_release_parent_reports_where_the_branch_went() {
    // The payload the design asks for. `parents_of` only ever reports bookmarks
    // pointing AT a parent, so the pure detector can only ever say "carries no
    // bookmark". Naming the branch and its new tip is the actionable half.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let stranded = lab.revision(&lab.work, "feat/alpha", "commit_id");

    // The branch moves on, leaving the old commit with nothing pointing at it.
    lab.jj_work(["new", "feat/alpha", "-m", "more work"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    let moved_to = lab.revision(&lab.work, "feat/alpha", "commit_id");
    assert_ne!(stranded.trim(), moved_to.trim());

    let past = knives::jj::branches_past(&lab.work, &knives::ids::CommitId::new(stranded.trim()))
        .expect("branches past");
    assert!(
        past.iter().any(|(branch, tip)| branch.as_str() == "feat/alpha" && tip.as_str() == moved_to.trim()),
        "expected feat/alpha reported at its new tip, got {past:?}"
    );
}

#[test]
fn a_foreign_pull_request_can_be_fetched_and_carried_as_a_release_parent() {
    // The design allows a release parent to be any upstream pull request, not
    // only our own branches. Without the objects locally that commit cannot be a
    // merge parent at all, and none of the obvious fetch routes work: jj brings
    // branches only, and importing a raw pull ref leaves the commit invisible.
    let lab = lab::Lab::new();
    lab.branch("feat/theirs", "theirs.txt", "someone else's work\n");
    lab.push_branch("feat/theirs");
    lab.publish_pull("feat/theirs", 42);
    let sha = lab.revision(&lab.work, "feat/theirs", "commit_id");

    // A clone that has never seen the branch, only the pull ref.
    let fetched = knives::jj::fetch_pull_ref(&lab.second, &lab.upstream.display().to_string(), 42)
        .expect("fetch pull ref");
    assert_eq!(fetched.as_str(), sha.trim(), "fetched the wrong commit");

    // And it is usable as a parent, which is the whole point.
    let trunk = knives::ids::CommitId::new(lab.revision(&lab.second, "main", "commit_id").trim());
    let merge = knives::jj::write_release(
        &lab.second,
        &knives::jj::ReleaseWrite {
            source: None,
            parents: &[trunk, fetched],
            message: Some("release: with a foreign PR"),
            bookmark: None,
            operation: "knives: cut with a foreign PR",
        },
    )
    .expect("merge");
    let parents = knives::jj::Repo::open(&lab.second)
        .expect("open")
        .parents_of(merge.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2);
}

#[test]
fn a_cut_refuses_when_release_like_described_work_lives_only_in_the_release_lineage() {
    // Given: a real hotfix uses a release-like description while stacked on the
    // old release. The next flat cut would not include it, and no keeper reaches it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "new",
        "release/2026-08-04",
        "-m",
        "chore(release): restore missing file",
    ]);
    std::fs::write(lab.work_path().join("hotfix.txt"), "fix\n").expect("write hotfix");
    lab.jj_work(["new"]); // park @ off the hotfix so it snapshots as its own commit
    let stacked = lab.revision(
        lab.work_path(),
        "description(glob:\"hotfix*\")",
        "commit_id",
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: a newer cut is attempted without acknowledgement.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: refused, naming the exact commit, and no new bookmark exists.
    assert!(!output.status.success(), "cut should have refused");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(&stacked.chars().take(12).collect::<String>()),
        "refusal must name the commit: {text}"
    );
    assert!(
        text.contains("--allow-drop"),
        "refusal must name the override: {text}"
    );
    let tips = Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );

    // And when: the operator states that dropping the hotfix is intended.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen after overridden cut")
        .bookmark_tips()
        .expect("read bookmark tips after overridden cut");
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05"),
        "the acknowledged cut must create the requested release"
    );
}

#[test]
fn a_dropped_branch_does_not_trip_the_orphan_gate() {
    // Given: a member dropped from the release. Its bookmark still holds its
    // content, so nothing is lost and the gate must stay quiet.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is made without --allow-drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it cuts, because feat/beta's bookmark still reaches its commits.
    assert!(
        output.status.success(),
        "gate tripped on a dropped branch: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // And: the drop it was given really removed something. A gate that stays
    // quiet over an unchanged release proves nothing about dropped content.
    let beta = commit_at(&lab, "feat/beta");
    assert!(
        !release_parent_commits(&lab, "release/2026-08-05").contains(&beta),
        "the dropped branch is still a parent of the successor cut"
    );
}

#[test]
fn include_adds_one_parent_and_changes_nothing_else() {
    // Given: a cut made before feat/gamma existed. Including gamma is one new
    // parent; every other parent stays at the commit the release already has.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    // When: the branch is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(
        output.status.success(),
        "include failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: exactly one parent was added and none moved.
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(after.len(), before.len() + 1, "{before:?} -> {after:?}");
    for parent in &before {
        assert!(
            after.contains(parent),
            "an existing parent moved: {before:?} -> {after:?}"
        );
    }
    let gamma = commit_at(&lab, "feat/gamma");
    assert!(after.contains(&gamma), "gamma tip missing: {after:?}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "gamma.txt"),
        "gamma\n"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "now has {} parent(s): included feat/gamma",
            after.len()
        )),
        "the reported parent count and delta must match the release: {stdout}"
    );
}

#[test]
fn a_cut_is_one_operation_described_for_the_op_log() {
    // Given: branches ready for a first cut. A cut used to be two operations
    // (build the merge, then name it after the audit) with a crash window
    // between them that stranded an anonymous merge; the audit now reads a
    // candidate that was never committed, so pass = one operation and
    // fail = none (#18).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let operations_before = operation_ids(&lab.work);

    // When: the release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        output.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: creating AND naming the audited release is ONE operation.
    let operations_after = operation_ids(&lab.work);
    assert_eq!(
        operations_after.len(),
        operations_before.len() + 1,
        "a cut must be one operation"
    );
    assert_eq!(
        newest_operation_description(&lab.work),
        "knives: cut release/2026-08-04"
    );
}

#[test]
fn a_discarded_candidate_leaves_no_trace() {
    // Given: a candidate cut that a failing audit would discard. The old flow
    // committed the merge before auditing, so a failure had to compensate with
    // an abandon — and a crash in between stranded an anonymous merge.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let operations_before = operation_ids(&lab.work);
    let visible_before = knives::jj::commits_matching(&lab.work, "all()").expect("list all");

    // When: the candidate is built, audit-shaped reads run against it, and it
    // is dropped without publishing.
    let mut candidate = knives::jj::candidate_release(
        &lab.work,
        knives::jj::CutSpec {
            source: None,
            parents: vec![alpha.clone(), beta],
            message: "release: doomed candidate".to_owned(),
        },
    )
    .expect("build candidate");
    let conflicted = candidate.conflicted_files().expect("list conflicts");
    assert!(conflicted.is_empty(), "{conflicted:?}");
    let replay = candidate
        .replay_outcome(trunk.as_str(), alpha.as_str())
        .expect("replay alpha onto the candidate");
    assert_eq!(replay, RebaseOutcome::Empty, "alpha must be carried");
    drop(candidate);

    // Then: no operation was written and no commit became visible.
    assert_eq!(operation_ids(&lab.work), operations_before);
    assert_eq!(
        knives::jj::commits_matching(&lab.work, "all()").expect("list all"),
        visible_before,
        "a discarded candidate leaked a commit"
    );
}

#[test]
fn a_release_edit_is_one_operation_described_for_the_op_log() {
    // Given: a cut release and a new branch to include. An include used to be
    // three operations (duplicate, describe, bookmark set), each described as
    // raw `args: jj ...` — hard to audit, and three reconciliation points with
    // concurrent agents (#18).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let operations_before = operation_ids(&lab.work);

    // When: the branch is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(
        output.status.success(),
        "include failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the edit is ONE operation, described as knives' own act.
    let operations_after = operation_ids(&lab.work);
    assert_eq!(
        operations_after.len(),
        operations_before.len() + 1,
        "an edit must be one operation"
    );
    let description = newest_operation_description(&lab.work);
    assert_eq!(
        description, "knives: release/2026-08-04: included feat/gamma",
        "the operation must describe the verb, not the plumbing"
    );
}

#[test]
fn an_edited_release_carries_the_repository_identity() {
    // Given: identity configured only in the repository's own jj config, the
    // way every lab and managed checkout carries it. A release merge written
    // with an empty author cannot be pushed by jj later, so the library-side
    // writer must resolve identity the way the jj CLI does.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");

    // When: the release is edited.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(output.status.success(), "{output:?}");

    // Then: the new release commit is authored, not anonymous.
    assert_eq!(
        lab.revision(&lab.work, "release/2026-08-04", "author.email()"),
        "knives-lab@example.test"
    );
    assert_eq!(
        lab.revision(&lab.work, "release/2026-08-04", "committer.name()"),
        "Knives Lab"
    );
}

#[test]
fn including_a_carried_branch_is_a_reported_noop() {
    // Including a parent the release already has changes nothing at all — not
    // the parent set, and not the release commit either. A no-op that still
    // duplicated the release would churn its identity under every consumer.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let before = release_parent_commits(&lab, "release/2026-08-04");
    let before_commit = commit_at(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("already carries feat/alpha"), "{stdout}");
    assert_eq!(release_parent_commits(&lab, "release/2026-08-04"), before);
    assert_eq!(
        commit_at(&lab, "release/2026-08-04"),
        before_commit,
        "a reported no-op rewrote the release"
    );
}

#[test]
fn include_refuses_to_advance_an_advanced_branch() {
    // Given: a released branch that has advanced. Moving a member to its tip is
    // a content change beyond "include this", so it only happens when asked
    // for by name: `advance`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("the branch has advanced")
            && stdout.contains("knives release advance feat/alpha"),
        "the refusal must name the verb that does move a member: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "include must not move a member"
    );
}

#[test]
fn drop_removes_one_parent_and_records_why() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let beta = repo.resolve_commit("feat/beta").expect("beta tip");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "beta is not ready"],
    );
    assert!(
        output.status.success(),
        "drop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dropped feat/beta: beta is not ready"),
        "the reported delta must carry the stated reason: {stdout}"
    );

    // Then: only beta's parent left; the reason is on the release itself; the
    // branch bookmark still holds the dropped work.
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(after.len(), before.len() - 1, "{before:?} -> {after:?}");
    assert!(!after.contains(&beta), "beta parent survived: {after:?}");
    assert!(after.contains(&alpha), "alpha parent vanished: {after:?}");
    let description = Repo::open(&lab.work)
        .expect("reopen")
        .description_of("release/2026-08-04")
        .expect("release description");
    assert!(description.contains("beta is not ready"), "{description}");
    let tips = Repo::open(&lab.work)
        .expect("reopen for tips")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "feat/beta"),
        "dropping a member must not touch its bookmark"
    );
}

#[test]
fn drop_resolves_an_advanced_branchs_parent_by_ancestry() {
    // Given: the release carries an old tip of feat/beta and the bookmark has
    // moved on. The name still resolves: the member parent is the one the
    // branch tip descends from.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released_beta = commit_at(&lab, "feat/beta");
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let repo = Repo::open(&lab.work).expect("reopen after beta advanced");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["drop", "feat/beta", "--why", "superseded"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        !after.contains(&released_beta),
        "the ancestor parent survived: {after:?}"
    );
    assert_eq!(after.len(), before.len() - 1, "{before:?} -> {after:?}");
    assert!(
        after.contains(&alpha),
        "the drop removed more than the named member: {after:?}"
    );
}

#[test]
fn advance_moves_only_the_named_parents_then_all() {
    // Given: both members have advanced. Advancing is a content change beyond
    // any single include, so it moves exactly what was named, and everything
    // only when nothing is.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let old_alpha = repo.resolve_commit("feat/alpha").expect("old alpha");
    let old_beta = repo.resolve_commit("feat/beta").expect("old beta");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let repo = Repo::open(&lab.work).expect("reopen");
    let new_alpha = repo.resolve_commit("feat/alpha").expect("new alpha");
    let new_beta = repo.resolve_commit("feat/beta").expect("new beta");

    // When: only alpha is advanced by name.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");

    // Then: alpha moved, beta did not.
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha),
        "the named member did not reach its tip: {parents:?}"
    );
    assert!(
        !parents.contains(&old_alpha),
        "the named member's old parent stayed: {parents:?}"
    );
    assert!(
        parents.contains(&old_beta),
        "advance moved an unnamed member: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "alpha.txt"),
        "alpha\nmore\n"
    );
    assert!(
        stdout.contains("advanced feat/alpha"),
        "the delta must name what moved: {stdout}"
    );
    let after_named = parents.len();

    // And: a bare advance moves every member that has advanced.
    let output = knives_release(&lab, &home, &["advance"]);
    assert!(output.status.success(), "{output:?}");
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_beta),
        "the bare advance left the advanced member behind: {parents:?}"
    );
    assert!(
        !parents.contains(&old_beta),
        "the bare advance kept the stale parent: {parents:?}"
    );
    assert_eq!(
        parents.len(),
        after_named,
        "a bare advance added or lost a parent: {parents:?}"
    );
    assert!(
        parents.contains(&new_alpha),
        "the bare advance moved the already-advanced member back: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "beta.txt"),
        "beta\nmore\n"
    );
}

#[test]
fn a_named_advance_claims_nothing_about_the_members_it_never_read() {
    // Given: one member already at its tip and one that has advanced. A named
    // advance looked only at what it was given, so "every member is at its
    // branch tip" is a fact it is not in a position to state.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    // When: only the member that has not moved is named.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    // Then: it reports alpha, stays quiet about beta, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("feat/alpha is already at its tip"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("every member of"),
        "a named advance claimed the sweep only a bare advance performs: {stdout}"
    );
    assert_eq!(release_parent_commits(&lab, "release/2026-08-04"), before);
}

#[test]
fn cut_with_a_previous_release_duplicates_it_verbatim() {
    // Given: a release, and a branch created after it. A cut is a new name for
    // the composition in hand, never a recomputation: the new branch joins
    // through `include`, and nothing advances.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let previous = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let parents = release_parent_commits(&lab, "release/2026-08-05");
    assert_eq!(parents, previous, "a cut must not recompute membership");
    let gamma = commit_at(&lab, "feat/gamma");
    assert!(
        !parents.contains(&gamma),
        "a branch must join through include, not by existing: {parents:?}"
    );
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    assert!(
        !Repo::open(&lab.work)
            .expect("reopen after the cut")
            .bookmark_tips()
            .expect("read bookmark tips")
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04"),
        "the reap was reported but the superseded bookmark survived"
    );
}

#[test]
fn a_fixed_scheme_cut_carries_the_local_release_in_hand() {
    // Given: a fixed release branch, published, then edited locally. The cut
    // must name what is here — duplicating the stale published position would
    // silently revert the unpushed include.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first fixed cut")
            .resolve_commit("integration")
            .is_ok(),
        "first fixed cut was not named: {first:?}"
    );
    lab.push_branch("integration");
    lab.fetch_work();
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(
        String::from_utf8_lossy(&included.stdout).contains("included feat/beta"),
        "{included:?}"
    );
    let repo = Repo::open(&lab.work).expect("open after the include");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let beta = repo.resolve_commit("feat/beta").expect("beta tip");
    let edited = repo
        .resolve_commit("integration")
        .expect("locally edited release tip");

    // When: the fixed branch is cut again.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: the cut ran, and the composition in hand survived it: a fresh
    // commit whose parents are still both members. This registry lists no
    // consumers, so pinned-ness is unknown and the run is incomplete — which is
    // not the cut refusing, so the assertions below hold it to having cut.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("cut integration as"), "{stdout}");
    let recut = commit_at(&lab, "integration");
    assert_ne!(recut, edited, "the fixed cut named nothing new: {stdout}");
    let parents = release_parent_commits(&lab, "integration");
    assert!(
        parents.contains(&beta),
        "the cut reverted an unpushed include: {parents:?}\n{stdout}"
    );
    assert!(
        parents.contains(&alpha),
        "the cut lost a member it started with: {parents:?}\n{stdout}"
    );
}

#[test]
fn an_edit_refuses_when_the_upstream_trunk_cannot_resolve() {
    // Given: a registry whose base names a branch upstream does not have.
    // Edits classify parents against the trunk; guessing with no trunk would
    // let a drop or advance touch the base, which is rebase's domain.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nbase = \"missing\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write broken-trunk registry");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("cannot resolve")
            && stdout.contains("release edits classify parents against the upstream trunk"),
        "the refusal must name the missing trunk as its reason: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "an edit ran without a resolvable trunk"
    );
}

#[test]
fn a_bare_advance_with_an_ambiguous_parent_changes_nothing() {
    // Given: one released parent with two advanced branches descending from it.
    // A bare advance promises every advanced member; advancing some while
    // skipping the ambiguous one would report success on a composition nobody
    // asked for.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released = commit_at(&lab, "feat/alpha");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\nmore\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new", released.as_str(), "-m", "rival follow-up"]);
    std::fs::write(lab.work.join("rival.txt"), "rival\n").expect("write rival");
    lab.jj_work(["bookmark", "create", "feat/rival", "-r", "@"]);
    lab.jj_work(["new"]);
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("nothing advanced"), "{stdout}");
    assert!(
        stdout.contains("several advanced branches")
            && stdout.contains("feat/alpha")
            && stdout.contains("feat/rival"),
        "the refusal must name the branches it could not choose between: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "an ambiguous bare advance mutated the release"
    );
}

#[test]
fn a_bare_advance_refuses_a_branch_that_would_replace_several_parents() {
    // Given: two released members, and a third branch built by merging both of
    // their released tips directly -- the shape of an integration branch built
    // across several former members, or of a member rebuilt with `jj
    // duplicate`, whose new tip has no ancestry back to its own stale parent
    // but happens to leave some *other* branch still reachable from it. Its
    // current tip descends from both stale parents, so each parent's ancestry
    // search finds it as the sole successor; deduping that result would
    // silently fold two distinct members into one.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = commit_at(&lab, "feat/alpha");
    let old_beta = commit_at(&lab, "feat/beta");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        before.contains(&old_alpha) && before.contains(&old_beta),
        "{before:?}"
    );
    lab.jj_work([
        "new",
        "feat/alpha",
        "feat/beta",
        "-m",
        "consolidated across both",
    ]);
    std::fs::write(lab.work.join("consolidated.txt"), "consolidated\n")
        .expect("write consolidated content");
    lab.jj_work(["bookmark", "create", "feat/consolidated", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("nothing advanced"), "{stdout}");
    assert!(
        stdout.contains("feat/consolidated")
            && stdout.contains("descends from 2 parents")
            && stdout.contains("drop and include instead"),
        "the refusal must name the overreaching branch and both parents it claimed: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "an overreaching bare advance mutated the release"
    );
}

#[test]
fn advance_from_recovers_a_branch_rebuilt_with_jj_duplicate() {
    // Given: two released members, then feat/alpha rebuilt the way `jj
    // duplicate` rebuilds a branch onto a new base -- same content, a fresh
    // change id sharing no ancestry with the commit the release still carries.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = commit_at(&lab, "feat/alpha");
    let old_beta = commit_at(&lab, "feat/beta");
    lab.jj_work(["new", "main", "-m", "alpha rebuilt onto a new base"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\n").expect("rebuild alpha content");
    lab.jj_work([
        "bookmark",
        "set",
        "feat/alpha",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);
    let rebuilt_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(
        rebuilt_alpha, old_alpha,
        "the rebuild must be a fresh commit"
    );

    // When: a plain named advance can't match it -- ancestry back to
    // `old_alpha` is gone -- so it is refused, not guessed at.
    let plain = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    let plain_stdout = String::from_utf8_lossy(&plain.stdout);
    assert_eq!(plain.status.code(), Some(3), "{plain_stdout}");
    assert!(
        plain_stdout.contains("carries no parent of feat/alpha"),
        "{plain_stdout}"
    );

    // But naming the exact old parent it replaces succeeds.
    let output = knives_release(
        &lab,
        &home,
        &["advance", "feat/alpha", "--from", old_alpha.as_str()],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("advanced feat/alpha"), "{stdout}");
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&rebuilt_alpha),
        "the rebuilt branch did not land: {parents:?}"
    );
    assert!(
        !parents.contains(&old_alpha),
        "the old parent stayed: {parents:?}"
    );
    assert!(
        parents.contains(&old_beta),
        "an unrelated member moved too: {parents:?}"
    );
    assert_eq!(parents.len(), 2, "{parents:?}");
}

#[test]
fn advance_from_requires_exactly_one_branch() {
    // Given: --from asserts one specific mapping. More than one named branch
    // makes that assertion ambiguous, so it is refused before touching
    // anything, not applied to the first branch and silently ignored for
    // the rest.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = commit_at(&lab, "feat/alpha");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &[
            "advance",
            "feat/alpha",
            "feat/beta",
            "--from",
            old_alpha.as_str(),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(2), "{stdout}");
    assert!(stdout.contains("give exactly one branch"), "{stdout}");
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "a rejected --from still mutated the release"
    );
}

#[test]
fn advance_from_refuses_when_the_named_commit_is_not_a_parent() {
    // Given: feat/alpha rebuilt onto a new base (so it is not already at its
    // released tip), and --from naming a commit that never was a member --
    // not even its own true old parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let stray = commit_at(&lab, "feat/gamma");
    lab.jj_work(["new", "main", "-m", "alpha rebuilt onto a new base"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\n").expect("rebuild alpha content");
    lab.jj_work([
        "bookmark",
        "set",
        "feat/alpha",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &["advance", "feat/alpha", "--from", stray.as_str()],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("is not a parent of release/2026-08-04"),
        "{stdout}"
    );
    assert!(
        stdout.contains("knives release include feat/alpha"),
        "{stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "a refused --from still mutated the release"
    );
}

#[test]
fn a_drop_without_a_why_is_a_usage_error() {
    // Dropping shipped content without a reason is how a release becomes
    // unexplainable later; the parser refuses rather than defaulting one in.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = knives_release(&lab, &home, &["drop", "feat/alpha"]);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "{output:?}"
    );
}

#[test]
fn include_by_commit_id_adds_that_exact_parent() {
    // A commit that no bookmark names is still includable by id.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/loose", "loose.txt", "loose\n");
    let loose = commit_at(&lab, "feat/loose");
    lab.jj_work(["bookmark", "forget", "feat/loose"]);
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", loose.as_str()]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        after.contains(&loose),
        "the raw commit id was not added as a parent: {after:?}"
    );
    assert_eq!(after.len(), before.len() + 1, "{before:?} -> {after:?}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "loose.txt"),
        "loose\n"
    );
}

#[test]
fn include_of_content_reachable_through_another_parent_reports_the_carrier() {
    // Given: beta stacked on alpha, alpha's own parent dropped. Alpha's content
    // still ships through beta's history, but membership is the parent set, so
    // include says which situation holds instead of pretending it happened.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["new", "feat/alpha", "-m", "stacked work"]);
    std::fs::write(lab.work.join("beta.txt"), "beta\n").expect("write beta");
    lab.jj_work(["bookmark", "create", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "beta carries it"],
    );
    assert!(dropped.status.success(), "{dropped:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    let before_commit = commit_at(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("through another parent's history"),
        "{stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "include mutated the release for content it does not carry"
    );
    assert_eq!(
        commit_at(&lab, "release/2026-08-04"),
        before_commit,
        "a reported non-include rewrote the release"
    );
}

#[test]
fn a_conflicted_cut_defers_reaping_the_resolution_carrier() {
    // Given: two members entangled in one file, so the release is conflicted.
    // A superseded cut is the record of how conflicts were last resolved:
    // reaping it while the successor is unresolved destroys the record exactly
    // when an abandon-and-recut would need it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first cut")
            .resolve_commit("release/2026-08-04")
            .is_ok(),
        "first cut was not named: {first:?}"
    );

    // When: a successor is cut while the conflicts stand.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the previous cut survives until the conflicts are resolved.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "kept release/2026-08-04: the live cut release/2026-08-05 still carries conflicts"
        ),
        "no deferral notice: {stdout}"
    );
    assert!(!stdout.contains("reaped release/2026-08-04"), "{stdout}");
    let tips = Repo::open(&lab.work)
        .expect("reopen after conflicted cut")
        .bookmark_tips()
        .expect("read bookmark tips");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        assert!(
            tips.keys()
                .any(|reference| reference.branch().as_str() == name),
            "{name} missing: {tips:?}"
        );
    }

    // And: an explicit `release reap` obeys the same gate. The cut's own output
    // points at this command, so an agent following it must not be able to
    // destroy the record either.
    let reap = knives_release(&lab, &home, &["reap"]);
    let reap_stdout = String::from_utf8_lossy(&reap.stdout);
    assert!(
        reap_stdout.contains("kept release/2026-08-04"),
        "reap said nothing about the deferral it made: {reap_stdout}"
    );
    assert!(
        !reap_stdout.contains("reaped release/2026-08-04"),
        "{reap_stdout}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after manual reap")
            .bookmark_tips()
            .expect("read bookmark tips")
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04"),
        "a manual reap destroyed the resolution carrier under a conflicted live cut"
    );
}

#[test]
fn a_bare_advance_under_the_fixed_scheme_ignores_the_release_bookmark() {
    // Given: a fixed release branch, whose own bookmark descends from every
    // member parent. A bare advance looks for bookmarks that moved past a
    // parent, so the release is the one bookmark it must never count as one:
    // "advancing" a member onto the release commit would make the release its
    // own member, and under this scheme the name never changes to reveal it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let first = knives_release(&lab, &home, &["cut"]);
    let released = Repo::open(&lab.work)
        .expect("open first fixed cut")
        .resolve_commit("integration")
        .unwrap_or_else(|error| panic!("first fixed cut was not named: {error}\n{first:?}"));
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced_alpha = commit_at(&lab, "feat/alpha");

    let output = knives_release(&lab, &home, &["advance"]);

    // Then: the member moved to its branch tip, and the release bookmark was
    // not mistaken for a branch that carries it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("advanced feat/alpha"), "{stdout}");
    let parents = release_parent_commits(&lab, "integration");
    assert!(parents.contains(&advanced_alpha), "{parents:?}");
    assert!(
        !parents.contains(&released),
        "the release bookmark was treated as a member's successor: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "integration", "alpha.txt"),
        "alpha\nmore\n"
    );
}

#[test]
fn a_member_landed_upstream_by_merge_commit_can_still_be_dropped() {
    // Given: a released member whose pull merged upstream WITH A MERGE COMMIT,
    // so its tip is now reachable from the trunk. Every parent is a member —
    // the base is never one — so landing must not dead-end the post-merge
    // `drop`, and the drop must say the release itself no longer carries it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let alpha = commit_at(&lab, "feat/alpha");
    let beta = commit_at(&lab, "feat/beta");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    assert!(
        Repo::open(&lab.work)
            .expect("open after merge")
            .is_ancestor(&alpha, &commit_at(&lab, "main@upstream"))
            .expect("ancestry answerable"),
        "fixture must land alpha in the trunk by merge commit"
    );

    // When: the landed member is dropped.
    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "merged upstream"],
    );

    // Then: it leaves; the other member stays; and because no remaining member
    // reaches alpha's content, the loss is stated.
    assert!(
        output.status.success(),
        "dropping a landed member dead-ended: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no remaining member carries feat/alpha's content"),
        "{stdout}"
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        !parents.contains(&alpha),
        "landed alpha survived: {parents:?}"
    );
    assert_eq!(parents, vec![beta], "only beta expected: {parents:?}");
}

#[test]
fn a_drop_whose_content_survives_through_another_member_stays_quiet() {
    // Given: beta stacked on alpha, both members. Dropping alpha loses nothing:
    // beta's ancestry still carries it, and saying otherwise would train people
    // to ignore the loss warning.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["new", "feat/alpha", "-m", "stacked work"]);
    std::fs::write(lab.work.join("beta.txt"), "beta\n").expect("write beta");
    lab.jj_work(["bookmark", "create", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "beta carries it"],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("loses it"),
        "content survives through beta; no loss to report: {stdout}"
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(parents, vec![commit_at(&lab, "feat/beta")], "{parents:?}");
}

#[test]
fn an_edit_before_any_cut_says_to_cut_one_first() {
    // Given: branches and no release at all. Membership is a release's parent
    // set, so there is nothing to edit yet, and an include that invented a
    // release would ship a composition that never passed the cut's gates.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("no release to edit; cut one first"),
        "{stdout}"
    );
    let tips = Repo::open(&lab.work)
        .expect("open after the refusal")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str().starts_with("release/")),
        "an edit invented a release: {tips:?}"
    );
}

#[test]
fn an_edit_refuses_when_every_pin_of_the_release_is_frozen() {
    // Given: a dated release whose only consumer pins it by revision. Editing
    // it in place reaches nobody, exactly as a rebase would not, so the edit is
    // refused in favour of the one remedy that reaches consumers: a new dated
    // cut. Editing anyway would report a change nothing consumes.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write frozen-pin registry");
    let before = release_parent_commits(&lab, release);

    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "frozen-pin guidance missing: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release),
        before,
        "a release no pin follows was edited in place"
    );
}

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

fn file_at_revision(lab: &Lab, revision: &str, file: &str) -> String {
    let output = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            revision,
            &format!("root:{file}"),
        ])
        .output()
        .expect("show revision file");
    assert!(
        output.status.success(),
        "file show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 file content")
}

#[derive(Clone, Copy)]
enum ReleaseOutput {
    Text,
    Json,
}

impl ReleaseOutput {
    const fn flag(self) -> &'static str {
        match self {
            Self::Text => "--text",
            Self::Json => "--json",
        }
    }
}

fn release_command(
    lab: &Lab,
    home: &tempfile::TempDir,
    output: ReleaseOutput,
    args: &[&str],
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args([output.flag(), "release", "--repo", "demo"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path());
    command
}

/// Run the knives binary's release command for the `demo` repo in `lab`.
fn knives_release(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Text, args)
        .output()
        .expect("run knives release")
}

/// Publish an origin bookmark from the second clone, then fetch it into `work`.
fn publish_remote_bookmark(lab: &Lab, source: &str, destination: &str) {
    let run_in_second = |args: &[&str]| {
        let command = Command::new("jj")
            .args(args)
            .current_dir(&lab.second)
            .output()
            .expect("run jj in second clone");
        assert!(
            command.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&command.stderr)
        );
    };
    run_in_second(&["git", "fetch", "--remote", "origin"]);
    run_in_second(&["bookmark", "create", destination, "-r", source]);
    run_in_second(&[
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        destination,
    ]);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
}

/// Add a commit to `branch`, leave its bookmark on the new tip, and step off it.
fn extend_branch(lab: &Lab, branch: &str, file: &str, content: &str) {
    lab.jj_work(["new", branch, "-m", "follow-up"]);
    std::fs::write(lab.work.join(file), content).expect("extend a branch");
    lab.jj_work(["bookmark", "set", branch, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The commit `revision` resolves to right now.
fn commit_at(lab: &Lab, revision: &str) -> CommitId {
    Repo::open(&lab.work)
        .expect("open to resolve a revision")
        .resolve_commit(revision)
        .expect("resolve revision")
}

/// [`release_test_home`], with `release/2026-08-04` already cut from whatever
/// branches `lab` carries: the starting point of every release-edit test.
fn home_after_first_cut(lab: &Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    let (home, consumer) = release_test_home(lab);
    let cut = knives_release(lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");
    (home, consumer)
}

/// The commit each parent of a named release sits at right now.
fn release_parent_commits(lab: &Lab, name: &str) -> Vec<CommitId> {
    Repo::open(&lab.work)
        .expect("open for release parents")
        .parents_of(name)
        .expect("read release parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect()
}

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

#[test]
fn a_bare_advance_ignores_a_branch_stacked_on_the_release() {
    // Given: a cut, and a branch somebody built on top of it — #4's third loss
    // mode's shape, and the reason `reap` refuses cuts with local descendants.
    // Such a branch descends from every member, so ancestry alone calls it their
    // advanced tip; advancing onto it would fold work nobody included into the
    // release and put the old cut in the new cut's own ancestry.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked on the cut"]);
    std::fs::write(lab.work.join("stacked.txt"), "stacked\n").expect("write stacked content");
    lab.jj_work(["bookmark", "create", "feat/stacked", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("every member of release/2026-08-04 is at its branch tip"),
        "a branch stacked on the release is not an advanced member: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "the bare advance folded a stacked branch into the release"
    );

    // And: the plan says what the branch actually is, rather than advising the
    // verb that refuses it.
    let plan = knives_release(&lab, &home, &[]);
    let planned = String::from_utf8_lossy(&plan.stdout);
    assert!(
        planned.contains("feat/stacked is stacked on release/2026-08-04"),
        "the plan called a stacked branch an advanced member: {planned}"
    );
}

#[test]
fn advancing_a_named_stacked_branch_is_refused() {
    // Given: the same stacked branch, named outright. Honouring it would make
    // the release contain its own predecessor, so it is answered, not improvised.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked on the cut"]);
    std::fs::write(lab.work.join("stacked.txt"), "stacked\n").expect("write stacked content");
    lab.jj_work(["bookmark", "create", "feat/stacked", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance", "feat/stacked"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("feat/stacked is stacked on release/2026-08-04"),
        "the refusal must say what is wrong with the branch: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "the refusal still moved a parent"
    );
}

#[test]
fn include_refuses_the_trunk_because_it_is_never_a_member() {
    // Given: a cut, and an upstream trunk that has moved past it. A release is
    // a flat merge of feature and fix branches; upstream enters through the
    // members' bases, never as a parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.advance_upstream("upstream advance\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "main@upstream"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("not a feature or fix branch"),
        "the refusal must state the model: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "including the trunk made it a parent"
    );
}

#[test]
fn an_edit_refuses_a_release_held_only_as_a_remote_ref() {
    // Given: a cut pushed to origin whose local bookmark is gone — the state a
    // fetch of somebody else's cut leaves, because jj creates no local bookmark
    // for an untracked remote one. An edit moves a local bookmark, and jj
    // rejects `name@remote` as a bookmark name, so without a gate the duplicate
    // is made and described before the move fails.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let before = release_parent_commits(&lab, "release/2026-08-04@origin");

    let output = knives_release(&lab, &home, &["include", "feat/beta"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("jj bookmark track release/2026-08-04@origin"),
        "the refusal must say how to get a local bookmark to edit: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04@origin"),
        before,
        "the remote-only release was edited anyway"
    );
}

#[test]
fn a_release_write_with_no_parents_is_refused_rather_than_done_in_place() {
    // A caller that computed an empty parent set would report the change it
    // meant to make while the composition stayed exactly as it was.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let source = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve alpha");

    let error = knives::jj::write_release(
        &lab.work,
        &knives::jj::ReleaseWrite {
            source: Some(&source),
            parents: &[],
            message: None,
            bookmark: None,
            operation: "knives: an empty edit",
        },
    )
    .expect_err("must refuse");

    assert!(
        error.to_string().contains("destination parent"),
        "the error must say what was missing: {error}"
    );
}

#[test]
fn advance_refuses_the_trunk_and_the_release_by_name() {
    // Given: a cut. A bare advance never treats the trunk or a release bookmark
    // as a branch that carries a member, and naming one must not reach the mover
    // it is kept away from: a member advanced onto the trunk gives the release a
    // second base, and one advanced onto a release makes it carry a whole cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    for named in ["main", "release/2026-08-04"] {
        let output = knives_release(&lab, &home, &["advance", named]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(3), "{named}: {stdout}");
        assert!(
            stdout.contains("is the trunk or a release name"),
            "{named} must be refused as unadvanceable: {stdout}"
        );
        assert_eq!(
            release_parent_commits(&lab, "release/2026-08-04"),
            before,
            "advancing {named} moved a parent"
        );
    }
}

#[test]
fn a_rebase_refuses_a_release_held_only_as_a_remote_ref() {
    // A rebase moves the release bookmark the same way an edit does, and it does
    // so after the duplicate has been made and described. With no local bookmark
    // to move, that leaves a described commit behind and fails on jj's bookmark
    // name parser instead of saying what is missing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.advance_upstream("upstream advance\n");
    let before = release_parent_commits(&lab, "release/2026-08-04@origin");

    let output = knives_release(&lab, &home, &["rebase"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("jj bookmark track release/2026-08-04@origin"),
        "the refusal must say how to get a local bookmark to move: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04@origin"),
        before,
        "the remote-only release was rebased anyway"
    );
}

#[test]
fn release_carries_answers_carried_for_a_member() {
    // Given: a release cut carrying alpha.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");

    // When: carries checks every release target and the upstream trunk.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the live release says it carries alpha exactly, so the answer is safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04"),
        "{stdout}"
    );
}

#[test]
fn release_carries_stops_before_superseded_targets_when_live_release_carries() {
    // Given: alpha is carried in both the previous cut and its live successor,
    // with the previous cut restored as a historical remote target afterward.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");

    // When: carries finds alpha in the live release or trunk census.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: that safe answer does not probe or print stale release targets.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-05"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("release/2026-08-04"),
        "safe results must not include superseded probes: {stdout}"
    );
}

#[test]
fn release_carries_answers_not_carried_for_outside_work() {
    // Given: a release cut carrying alpha and an independent beta branch.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    // When: beta is checked against every release target and the trunk.
    let output = knives_release(&lab, &home, &["carries", "feat/beta"]);

    // Then: no safe target carries it, so the answer names the real remaining diff.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
}

#[test]
fn release_carries_in_checks_only_the_requested_target() {
    // Given: alpha is in the release but absent from the upstream trunk.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");

    // When: the explicit target is the upstream trunk.
    let output = knives_release(
        &lab,
        &home,
        &["carries", "feat/alpha", "--in", "main@upstream"],
    );

    // Then: the live release cannot make a single-target trunk query safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("NOT carried        main@upstream"),
        "{stdout}"
    );
    assert!(!stdout.contains("release/2026-08-04"), "{stdout}");
}

#[test]
fn release_carries_in_exits_successfully_when_the_selected_historical_release_carries() {
    // Given: alpha was carried by a release that later became superseded.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");

    // When: the explicit target is the known, historical release.
    let output = knives_release(
        &lab,
        &home,
        &["carries", "feat/alpha", "--in", "release/2026-08-04@origin"],
    );

    // Then: --in reports the direct target verdict, not its safe-delete role.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04@origin"),
        "{stdout}"
    );
}

#[test]
fn release_carries_answers_against_the_trunk_when_no_release_exists() {
    // Given: a branch but no release in hand.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: carries has no explicit target.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the orphan question is answered against the upstream trunk.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("main@upstream"), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
}

#[test]
fn release_carries_reports_carried_rewritten_for_a_squash_landed_branch() {
    // Given: alpha is squash-landed, so the trunk has its content but not its tip.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let alpha = commit_at(&lab, "feat/alpha");
    let trunk = commit_at(&lab, "main@upstream");
    assert!(
        !Repo::open(&lab.work)
            .expect("open after squash merge")
            .is_ancestor(&alpha, &trunk)
            .expect("ancestry answerable"),
        "fixture must use a rewritten trunk commit"
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: alpha is checked without any release.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the trunk's tree-content evidence proves its rewritten carriage.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-rewritten  main@upstream"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&trunk.as_str().chars().take(12).collect::<String>()),
        "{stdout}"
    );
}

#[test]
fn carries_superseded_only_carriage_is_findings() {
    // Given: a published cut carrying alpha survives at origin after the local
    // release drops it and the next cut becomes live.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let original = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        original.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let dropped = knives_release(
        &lab,
        &home,
        &[
            "drop",
            "feat/alpha",
            "--why",
            "superseded release preserves it",
        ],
    );
    assert!(dropped.status.success(), "{dropped:?}");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    assert!(second.status.success(), "{second:?}");

    // Publish the preserved historical commit under its release name only after
    // the successor cut has passed the duplicate-release gate.
    let run_in_second = |args: &[&str]| {
        let command = Command::new("jj")
            .args(args)
            .current_dir(&lab.second)
            .output()
            .expect("run jj in second clone");
        assert!(
            command.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&command.stderr)
        );
    };
    run_in_second(&["git", "fetch", "--remote", "origin"]);
    run_in_second(&[
        "bookmark",
        "create",
        "release/2026-08-04",
        "-r",
        "history/alpha-release@origin",
    ]);
    run_in_second(&[
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);

    // When: bare carries finds alpha only in the historical remote cut.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the historical row is visible, but it cannot make the result safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04@origin"),
        "{stdout}"
    );
    assert!(
        stdout.contains("NOT carried        release/2026-08-05"),
        "{stdout}"
    );
}

/// A release history with one live and one superseded cut, plus one branch
/// deliberately absent from both. The historical cut remains only as an origin
/// ref, so it is a census target without becoming a maintained branch itself.
fn census_lab() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "history/alpha-release"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    (lab, home)
}

fn census_block<'a>(stdout: &'a str, branch: &str) -> &'a str {
    let wanted = format!("  {branch} @");
    stdout
        .split("\n\n")
        .find(|block| block.starts_with(&wanted))
        .unwrap_or_else(|| panic!("no census block for {branch}: {stdout}"))
}

#[test]
fn census_finds_the_orphan_branch() {
    // Given: alpha is in the live cut and beta is independent; the preceding
    // dated cut survives only as a superseded origin target.
    let (lab, home) = census_lab();

    // When: census asks only local carriage questions, so PR state is explicitly unknown.
    let output = knives_release(&lab, &home, &["carries", "--all", "--no-github"]);

    // Then: a carried member names its live carrier but does not spend a
    // superseded probe, while a non-carried member proves the negative against
    // every target, including the historical release.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        !stdout
            .split("\n\n")
            .any(|block| block.starts_with("  main @")),
        "the upstream trunk is a target, never a maintained-branch row: {stdout}"
    );
    let alpha = census_block(&stdout, "feat/alpha");
    assert!(
        alpha.contains("carried-exact      release/2026-08-05"),
        "{alpha}"
    );
    assert!(
        !alpha.contains("release/2026-08-04"),
        "a live-carried row must not probe superseded targets: {alpha}"
    );
    let beta = census_block(&stdout, "feat/beta");
    for target in [
        "release/2026-08-05",
        "main@upstream",
        "release/2026-08-04@origin",
    ] {
        assert!(
            beta.contains(&format!("NOT carried        {target}")),
            "beta must be checked against {target}: {beta}"
        );
    }
    assert!(
        stdout.contains("orphans: not carried anywhere (pull request state unknown)\n  feat/beta"),
        "{stdout}"
    );
}

#[test]
fn census_marks_unknown_pull_orphans_as_unanswered_in_json() {
    // Given: beta is locally uncarried and pull-request lookup is deliberately skipped.
    let (lab, home) = census_lab();

    // When: the census is emitted as its machine report.
    let output = knives_release_json(&lab, &home, &["carries", "--all", "--no-github"]);

    // Then: the qualified text listing remains actionable, but JSON cannot
    // represent beta as a pull-safe, proven orphan.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert!(beta["orphan"].is_null(), "{report}");
    assert!(
        report["orphans"]
            .as_array()
            .expect("qualified orphan listing")
            .iter()
            .any(|orphan| orphan == "feat/beta"),
        "{report}"
    );
    assert_eq!(output.status.code(), Some(3), "{report}");
}

#[test]
fn census_respects_an_open_pull() {
    // Given: beta's content remains outside every release but the forge says its
    // branch has an open pull request.
    let (lab, home) = census_lab();
    let pulls = format!("[{}]", pull_record(17, "OPEN", "feat/beta", None));

    // When: the real CLI completes one forge snapshot for the census.
    let output = knives_release_json_with_forge(&lab, &home, &pulls, &["carries", "--all"]);

    // Then: the branch association is retained in the report and forbids an
    // orphan result despite every target being non-carried.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert_eq!(beta["in_open_pull"], true, "{report}");
    assert_eq!(beta["orphan"], false, "{report}");
    let alpha = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .expect("alpha row");
    assert_eq!(
        alpha["in_open_pull"], false,
        "a completed snapshot answers that an absent branch has no open pull: {report}"
    );
    assert_eq!(output.status.code(), Some(0), "{report}");
}

#[test]
fn census_withholds_a_selected_pull_fact_as_unanswered() {
    // Given: beta is locally uncarried and discovery names its open pull request.
    let (lab, home) = census_lab();
    let pulls = format!("[{}]", pull_record(17, "OPEN", "feat/beta", None));

    // When: the live batch withholds that selected pull request's fact.
    let output = knives_release_json_with_forge_withheld_facts(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[17],
        args: &["carries", "--all"],
    });

    // Then: discovery cannot make the pull state a deletion-safe answer.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert!(beta["in_open_pull"].is_null(), "{report}");
    assert!(beta["orphan"].is_null(), "{report}");
    assert_eq!(output.status.code(), Some(3), "{report}");
}

#[test]
fn census_keeps_local_orphans_when_the_forge_is_unavailable() {
    // Given: beta is locally uncarried and the forge refuses every request.
    let (lab, home) = census_lab();

    // When: census attempts the normal forge snapshot.
    let output = knives_release_with_failing_forge(&lab, &home, &["carries", "--all"]);

    // Then: failure changes pull-request knowledge to unknown without hiding
    // the local orphan finding, and the unanswered deletion-safety check wins.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("pull request state unavailable:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("orphans: not carried anywhere (pull request state unknown)\n  feat/beta"),
        "{stdout}"
    );
}

#[test]
fn census_excludes_anonymous_heads() {
    // Given: an unbookmarked commit with unique content, disconnected from the
    // working copy before the census runs.
    let (lab, home) = census_lab();
    lab.jj_work(["new", "main@upstream", "-m", "stranded"]);
    std::fs::write(lab.work.join("stranded.txt"), "stranded\n").expect("write stranded content");
    lab.jj_work(["new", "main@upstream"]);

    // When: the maintained-branch census runs without a pull-request lookup.
    let output = knives_release_json(&lab, &home, &["carries", "--all", "--no-github"]);

    // Then: the unnamed head is audit population, not a census row or schema field.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let branches = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .map(|row| row["branch"].as_str().expect("branch name"))
        .collect::<Vec<_>>();
    assert_eq!(branches, ["feat/alpha", "feat/beta"], "{report}");
    assert!(
        report.get("anonymous").is_none(),
        "anonymous heads belong exclusively to audit: {report}"
    );
}

/// Run census without a forge so its locally discovered anonymous id can be
/// supplied as a pull request's exact head oid on a subsequent run.
fn knives_release_json(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Json, args)
        .output()
        .expect("run knives release census")
}

/// Run census with the full snapshot forge protocol and ask the CLI for JSON so
/// tests assert the report's machine contract rather than parsing prose.
fn knives_release_json_with_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    pulls: &str,
    args: &[&str],
) -> std::process::Output {
    knives_release_json_with_forge_withheld_facts(ReleaseWithSnapshotForgeInput {
        lab,
        home,
        pulls,
        withheld_facts: &[],
        args,
    })
}

fn knives_release_json_with_forge_withheld_facts(
    input: ReleaseWithSnapshotForgeInput<'_>,
) -> std::process::Output {
    release_with_snapshot_forge(input, ReleaseOutput::Json)
}

/// Run census with a forge that fails before returning any data.
fn knives_release_with_failing_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    args: &[&str],
) -> std::process::Output {
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_failing_gh(shim.path(), &shim.path().join("calls.log"));
    release_command(lab, home, ReleaseOutput::Text, args)
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run knives release census with a failing forge")
}

fn knives_pr_with_shim(
    number: u64,
    timeline: bool,
    pulls: &str,
    timeline_nodes: Option<&str>,
) -> std::process::Output {
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh_with_timeline(shim.path(), pulls, timeline_nodes);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command
        .args(["--text", "pr"])
        .arg(number.to_string())
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()));
    if timeline {
        command.arg("--timeline");
    }
    command.output().expect("run knives pr with a forge shim")
}

#[test]
fn pr_reports_a_closed_pull_and_its_branch_through_the_real_binary() {
    let pulls = format!("[{}]", pull_record(7, "CLOSED", "feat/closed", None));

    let output = knives_pr_with_shim(7, false, &pulls, None);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Ok.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("#7"), "stdout: {stdout}");
    assert!(stdout.contains("CLOSED"), "stdout: {stdout}");
    assert!(stdout.contains("feat/closed"), "stdout: {stdout}");
}

#[test]
fn pr_reports_an_unanswered_number_as_incomplete_through_the_real_binary() {
    let pulls = format!("[{}]", pull_record(7, "CLOSED", "feat/closed", None));

    let output = knives_pr_with_shim(999, false, &pulls, None);

    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("999"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pr_timeline_renders_force_pushes_with_both_tree_oids_through_the_real_binary() {
    let pulls = format!("[{}]", pull_record(7, "CLOSED", "feat/closed", None));
    let timeline = r#"[{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-08-30T22:41:02Z",
        "beforeCommit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "tree":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},
        "afterCommit":{"oid":"cccccccccccccccccccccccccccccccccccccccc",
        "tree":{"oid":"dddddddddddddddddddddddddddddddddddddddd"}}}]"#;

    let output = knives_pr_with_shim(7, true, &pulls, Some(timeline));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("force-push"), "stdout: {stdout}");
    assert!(stdout.contains("tree bbbbbbbbbbbb"), "stdout: {stdout}");
    assert!(stdout.contains("tree dddddddddddd"), "stdout: {stdout}");
}

/// Registry home for commands that reconcile the lab's live bare remotes.
fn mutation_test_home(lab: &Lab, release: Option<&std::path::Path>) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    let release = release.map_or_else(String::new, |path| {
        format!("release = \"{}\"\n", path.display())
    });
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"{}\"\n{release}",
            lab.work.display(),
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("write registry");
    home
}

/// Run the reconciliation command against the lab's registry entry.
fn knives_pushed(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--json", "pushed"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("run knives pushed")
}

#[test]
fn pushed_confirms_a_pushed_branch_and_flags_an_unpushed_one() {
    // Given: alpha reached the live origin and beta exists only in the checkout.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.push_branch("feat/alpha");
    let home = mutation_test_home(&lab, None);

    // When: every local bookmark is reconciled against its owning remote.
    let output = knives_pushed(&lab, &home, &["--repo", "demo"]);

    // Then: the missing beta ref is a finding while alpha is confirmed live.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("pushed emits JSON");
    let rows = report["rows"].as_array().expect("rows");
    let alpha = rows
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .expect("alpha row");
    let beta = rows
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert_eq!(alpha["verdicts"][0]["verdict"], "in-sync");
    assert_eq!(beta["verdicts"][0]["verdict"], "not-on-remote");
    assert_eq!(beta["verdicts"][0]["remote"], "origin");
}

#[test]
fn pushed_catches_the_no_op_delete() {
    // Given: alpha was pushed before its local bookmark was removed.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "delete", "feat/alpha"]);
    let home = mutation_test_home(&lab, None);

    // When: the named, now-local-absent branch is reconciled.
    let output = knives_pushed(&lab, &home, &["feat/alpha", "--repo", "demo"]);

    // Then: the live ref is reported rather than silently accepting the delete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("pushed emits JSON");
    let row = report["rows"][0].as_object().expect("one row");
    assert!(row.get("local").is_none(), "was: {row:?}");
    assert_eq!(row["verdicts"][0]["verdict"], "remote-only");
    assert_eq!(row["verdicts"][0]["remote"], "origin");
}

#[test]
fn pushed_compares_a_tracked_pull_head() {
    // Given: alpha's tracked pull ref still names the older trunk commit.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let trunk = Repo::open(&lab.work)
        .expect("open lab")
        .resolve_commit("main@origin")
        .expect("resolve trunk");
    let status = Command::new("git")
        .args(["update-ref", "refs/pull/7/head", trunk.as_str()])
        .current_dir(lab.temp_origin())
        .status()
        .expect("write pull fixture");
    assert!(status.success(), "write pull fixture");
    let home = mutation_test_home(&lab, None);
    let tracked = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "track",
            "feat/alpha",
            "--pr",
            "7",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("track pull");
    assert!(
        tracked.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&tracked.stderr)
    );

    // When: pushed compares the stated pull head from origin.
    let output = knives_pushed(&lab, &home, &["feat/alpha", "--repo", "demo"]);

    // Then: the independent pull-head mismatch is surfaced.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("pushed emits JSON");
    let verdicts = report["rows"][0]["verdicts"].as_array().expect("verdicts");
    assert!(
        verdicts
            .iter()
            .any(|verdict| verdict["verdict"] == "pull-head-differs" && verdict["number"] == 7),
        "was: {verdicts:?}"
    );
}

#[test]
fn pushed_partitions_release_names_to_the_release_remote() {
    // Given: the release and origin roles point at separate bare remotes.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("release/2026-08-04", "release.txt", "release\n");
    lab.push_branch("feat/alpha");
    let home = tempfile::tempdir().expect("create release remote home");
    let release = home.path().join("release.git");
    let status = Command::new("git")
        .args(["init", "--bare", release.to_str().expect("utf-8 path")])
        .status()
        .expect("create release remote");
    assert!(status.success(), "create release remote");
    lab.jj_work([
        "git",
        "remote",
        "add",
        "release",
        release.to_str().expect("utf-8 path"),
    ]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "release",
        "--bookmark",
        "release/2026-08-04",
    ]);
    let config = mutation_test_home(&lab, Some(&release));

    // When: both roles contain only the ref class they own.
    let synced = knives_pushed(&lab, &config, &["--repo", "demo"]);

    // Then: cross-remote absence is topology, so both refs are in sync.
    assert!(
        synced.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&synced.stdout).expect("pushed emits JSON");
    for branch in ["feat/alpha", "release/2026-08-04"] {
        let row = report["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["branch"] == branch)
            .expect("row");
        assert_eq!(row["verdicts"][0]["verdict"], "in-sync", "row: {row}");
    }

    // When: the release remote moves its release ref to a different live commit.
    let trunk = Repo::open(&lab.work)
        .expect("open lab")
        .resolve_commit("main@origin")
        .expect("resolve trunk");
    let status = Command::new("git")
        .args([
            "update-ref",
            "refs/heads/release/2026-08-04",
            trunk.as_str(),
        ])
        .current_dir(&release)
        .status()
        .expect("move release ref");
    assert!(status.success(), "move release ref");
    let drifted = knives_pushed(&lab, &config, &["release/2026-08-04", "--repo", "demo"]);

    // Then: only its owner role names the mismatch.
    assert_eq!(
        drifted.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&drifted.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&drifted.stdout).expect("pushed emits JSON");
    assert_eq!(report["rows"][0]["verdicts"][0]["verdict"], "differs");
    assert_eq!(report["rows"][0]["verdicts"][0]["remote"], "release");
}

/// Run estate reconciliation against the lab's registry entry.
fn knives_audit(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--json", "audit"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("run knives audit")
}

#[test]
fn audit_reports_zombie_drift_and_anonymous_heads() {
    // Given: one locally rewritten pushed branch, one remote-only branch, and
    // a described anonymous head no workspace currently holds.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/zombie", "zombie.txt", "zombie\n");
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/zombie");
    lab.rewrite_local_branch("feat/alpha", "locally moved\n");
    lab.jj_work(["bookmark", "delete", "feat/zombie"]);
    lab.jj_work(["new", "main@origin", "-m", "stranded"]);
    lab.jj_work(["new", "main@origin"]);
    let home = mutation_test_home(&lab, None);

    // When: audit runs without a forge session.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: each independently recoverable estate fact remains a separate
    // finding, while the skipped pull-head reconciliation makes the result incomplete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    let kinds = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| finding["kind"].as_str().expect("finding kind"))
        .collect::<Vec<_>>();
    for expected in ["remote-drift", "zombie-branch", "orphan-commit"] {
        assert!(kinds.contains(&expected), "missing {expected}: {report}");
    }
}

#[test]
fn audit_does_not_treat_a_shared_release_url_as_a_separate_zombie_remote() {
    // Given: release is configured to the same remote as origin, where alpha
    // remains after its local bookmark is deleted.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "delete", "feat/alpha"]);
    let release = lab.temp_origin();
    let home = mutation_test_home(&lab, Some(&release));

    // When: audit classifies the one shared remote.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: origin's missing bookmark is a zombie once, never a second release
    // zombie; skipped pull-head reconciliation makes the result incomplete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    let zombies = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["kind"] == "zombie-branch")
        .collect::<Vec<_>>();
    assert_eq!(zombies.len(), 1, "was: {report}");
    assert!(
        zombies[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("origin has feat/alpha")),
        "was: {zombies:?}"
    );
}

#[test]
fn audit_reports_release_drift_from_the_recorded_cut() {
    // Given: a cut records its created commit, then its bookmark moves sideways.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = mutation_test_home(&lab, None);
    let cut = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "release",
            "--repo",
            "demo",
            "cut",
            "release/2026-08-04",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("cut release");
    assert!(
        cut.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&cut.stdout),
        String::from_utf8_lossy(&cut.stderr)
    );
    knives::jj::set_bookmark_anywhere(&lab.work, "release/2026-08-04", "feat/alpha")
        .expect("move local release sideways");
    lab.jj_work(["workspace", "update-stale"]);

    // When: audit compares the release's current tip to its newest record.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: the recorded cut disagreement remains a content finding, but the
    // skipped pull-head reconciliation prevents a completed result.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|finding| finding["kind"] == "release-drift"),
        "was: {report}"
    );
}

#[test]
fn audit_with_no_github_still_reconciles() {
    // Given: a local-only branch and no forge transport.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = mutation_test_home(&lab, None);

    // When: the optional forge check is disabled.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: local reconciliation still reports its remote fact, while the
    // skipped open-pull reconciliation is an unanswered question.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|finding| finding["kind"] == "remote-drift"),
        "was: {report}"
    );
    assert!(
        report["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .any(|problem| problem
                .as_str()
                .is_some_and(|problem| problem.contains("pull-head reconciliation was skipped"))),
        "was: {report}"
    );
}
