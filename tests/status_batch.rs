//! `knives status` asks the forge once.
//!
//! One facts batch answers review age, checks, stated pulls and dependencies
//! for every branch; a failed batch clears what it would have answered rather
//! than inventing it. Landed verdicts come from the cache when the key matches,
//! an unresolvable trunk fails loudly and touches no cache, and the probes
//! answer the same in parallel as in series.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
#[path = "common/pulls.rs"]
mod pulls;

use knives::commands::status::{self};
use knives::config::Registry;
use knives::forge::{
    ChecksSummary, Forge, ForgeError, PullFacts, PullRequest, PullSummary, RepoIdentity,
    SweepEntry, SweepPage, TimelineEvent,
};
use knives::ids::BranchName;
use knives::jj::Repo;
use knives::store::Store;
use lab::{extend_branch, lab_entry, without_forge_elapsed};
use pulls::pull_request_with_head;
use std::collections::BTreeMap;

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
    let alpha = row("feat/alpha");
    assert_eq!(alpha.state, status::BranchState::ChecksFailing);
    assert_eq!(alpha.review.as_deref(), Some("changes-requested"));
    assert_eq!(alpha.checks.as_deref(), Some("failing"));
    assert!(
        alpha.flags.iter().any(|flag| flag == "review-stale"),
        "was: {alpha:?}"
    );

    let beta = row("feat/beta");
    assert_eq!(beta.state, status::BranchState::Approved);
    assert_eq!(beta.review.as_deref(), Some("approved"));
    assert_eq!(
        beta.checks.as_deref(),
        Some("none-ran"),
        "consulted with nothing running is not the same as unconsulted"
    );
    assert!(
        !beta.flags.iter().any(|flag| flag == "review-stale"),
        "was: {beta:?}"
    );

    // And: a settled pull request is neither asked about nor reported on
    let gamma = row("feat/gamma");
    assert_eq!(gamma.state, status::BranchState::Closed);
    assert_eq!(gamma.review, None);
    assert_eq!(gamma.checks, None);
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

    // Then: the report's facts are unchanged; its measured forge duration is
    // specific to each run, and the total covers the phases it timed.
    assert_eq!(
        without_forge_elapsed(&status::render::render(&plain, true)),
        without_forge_elapsed(&status::render::render(&measured, true)),
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
    let mut pull = pull_request_with_head(11, "OPEN", "feat/alpha", "head-11");
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

    assert!(!report.forge.consulted, "was: {report:?}");
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("pull request state unavailable")),
        "was: {report:?}"
    );
    assert_eq!(status::exit_for(&report), knives::cli::Exit::Incomplete);
    let row = &report.branches[0];
    assert_eq!(row.state, status::BranchState::Unknown);
    assert!(row.pr.is_none(), "no live-looking pull request cell");
    assert_eq!(row.review, None, "a refused answer is not current");
    assert_eq!(row.checks, None, "a refused answer is not no checks");
    assert!(
        !row.flags.iter().any(|flag| flag == "review-stale"),
        "a refused answer is not a stale review: {row:?}"
    );
}

#[test]
fn a_consulted_false_report_carries_an_unanswered_stated_pull() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let target = knives::ids::BranchTarget::new(name.clone(), BranchName::new("feat/alpha"));
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(11, "OPEN", "feat/alpha", "head-11"),
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

    assert!(!report.forge.consulted, "was: {report:?}");
    let row = &report.branches[0];
    assert_eq!(row.state, status::BranchState::Unknown);
    assert_eq!(
        row.pr
            .as_ref()
            .map(|pull| (pull.number, pull.state.as_str(), pull.stated)),
        Some((42, "unknown", Some(true))),
        "the failed facts batch must not turn the stated pull into a live fact: {report:?}"
    );
    assert_eq!(row.review, None, "a failed facts batch has no review fact");
    assert_eq!(row.checks, None, "a failed facts batch has no check fact");
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
            pull_request_with_head(11, "OPEN", "feat/alpha", "head-11"),
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

    let pull = report.branches[0]
        .pr
        .as_ref()
        .expect("an inferred pull request");
    assert_eq!(
        (pull.number, pull.state.as_str(), pull.stated),
        (11, "open", None),
        "the inferred pull is the primary cell: {report:?}"
    );
    assert_eq!(
        pull.prior
            .iter()
            .map(|prior| (prior.number, prior.state.as_str()))
            .collect::<Vec<_>>(),
        vec![(42, "closed")],
        "the stated number did not come from the snapshot: {report:?}"
    );
    assert!(
        report.findings.is_empty(),
        "a merged dependency became unmet: {report:?}"
    );
    assert!(report.problems.is_empty(), "was: {report:?}");
    assert!(
        status::render::render(&report, true).contains("#11 prior #42 closed"),
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

    // Then: apart from each run's elapsed forge duration, not one report token
    // differs, including the landed column.
    assert_eq!(
        without_forge_elapsed(&status::render::render(&serial, true)),
        without_forge_elapsed(&status::render::render(&parallel, true)),
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
