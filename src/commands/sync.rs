//! `knives sync`: fetch, then classify what happened to each tracked pull request.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::cli::Exit;
use crate::config::{RepoEntry, Role};
use crate::forge::{Forge, PullFacts, PullSummary};
use crate::ids::{BranchName, BranchTarget};
use crate::jj::{fetch_all, fetch_pull_ref, pull_heads};
use crate::ledger::Scribe;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullState {
    Unchanged,
    Advanced,
    /// Seen for the first time; nothing to compare it against yet.
    New,
    Merged,
    Closed,
}

impl PullState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::New => "new",
            Self::Advanced => "advanced",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

impl fmt::Display for PullState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What happened to a tracked pull request since the last run.
///
/// Forge state wins over movement: a merged pull request whose head also moved
/// is merged, and calling it merely advanced keeps us carrying work that is
/// already upstream.
pub fn classify_pull(previous: Option<&str>, current: &str, state: &str) -> PullState {
    match state.to_ascii_uppercase().as_str() {
        "MERGED" => PullState::Merged,
        "CLOSED" => PullState::Closed,
        // A first sighting is not "nothing moved". Reporting them the same way made
        // the first run against a fresh state file look like a quiet no-op.
        _ => match previous {
            None => PullState::New,
            Some(seen) if seen != current => PullState::Advanced,
            Some(_) => PullState::Unchanged,
        },
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Row {
    pub number: u64,
    pub label: String,
    pub state: PullState,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub repo: String,
    pub rows: Vec<Row>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

pub struct SyncInput<'a> {
    pub entry: &'a RepoEntry,
    pub store: &'a mut Store,
    pub forge: Option<&'a dyn Forge>,
    pub scribe: &'a Scribe,
    pub cache: Option<&'a std::path::Path>,
}

impl fmt::Debug for SyncInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncInput")
            .field("has_forge", &self.forge.is_some())
            .field("has_cache", &self.cache.is_some())
            .finish()
    }
}

/// Fetch the objects for foreign pull requests we carry as release parents.
///
/// Ours need nothing: we already have the branch. Theirs cannot be a release
/// parent at all without this, which is the whole reason the design allows a
/// release parent to be any upstream pull request. Extracted from `sync_repo`
/// because that function had grown past what one reviewer holds at once.
fn fetch_foreign(
    repo: &std::path::Path,
    upstream: &str,
    foreign: &BTreeSet<u64>,
    report: &mut Report,
) {
    for number in foreign {
        match fetch_pull_ref(repo, upstream, *number) {
            Ok(commit) => report.notes.push(format!(
                "fetched foreign #{number} as pull/{number} ({})",
                commit.as_str().chars().take(12).collect::<String>()
            )),
            Err(error) => report
                .problems
                .push(format!("could not fetch foreign #{number}: {error}")),
        }
    }
}

/// Which pull requests are ours to track.
///
/// Ours means: a pull request whose head is a branch we carry, plus foreign ones
/// we deliberately carry as release parents, plus anything tracked before. Every
/// open pull request on the upstream is not our business and buries the signal:
/// on a real repository that was 10 rows against 83.
pub fn tracked_pull_requests(
    pull_requests: &[PullSummary],
    foreign: &BTreeSet<u64>,
    seen: &BTreeMap<String, String>,
) -> BTreeMap<u64, String> {
    let mut tracked: BTreeMap<u64, String> = BTreeMap::new();
    for pr in pull_requests {
        let _ = tracked.insert(pr.number, pr.head_ref_name.clone());
    }
    for key in seen.keys() {
        if let Ok(number) = key.parse::<u64>() {
            let _ = tracked
                .entry(number)
                .or_insert_with(|| format!("#{number}"));
        }
    }
    for number in foreign {
        let _ = tracked
            .entry(*number)
            .or_insert_with(|| format!("#{number} (foreign)"));
    }
    tracked
}

/// The current head from fetched pull refs, or the live forge fact when no ref exists.
fn current_pull_head<'a>(
    heads: &'a BTreeMap<u64, String>,
    fact: Option<&'a PullFacts>,
    number: u64,
) -> Option<&'a str> {
    heads
        .get(&number)
        .map(String::as_str)
        .or_else(|| fact.map(|fact| fact.pull.head_ref_oid.as_str()))
}

/// What to record about a pull request that moved, and nothing for one that did not.
///
/// `unchanged` is the absence of an event, and `new` is a first sighting rather
/// than something that happened: recording either would fill a fork's history
/// with one line per pull request per run.
fn transition_text(number: u64, state: PullState, head: &str) -> Option<String> {
    match state {
        PullState::Merged => Some(format!("#{number} merged")),
        PullState::Closed => Some(format!("#{number} closed")),
        PullState::Advanced => Some(format!(
            "#{number} advanced to {}",
            head.chars().take(12).collect::<String>()
        )),
        PullState::Unchanged | PullState::New => None,
    }
}

/// The classified state and observed head for one tracked pull request.
#[derive(Clone, Copy)]
struct PullTransition<'a> {
    number: u64,
    state: PullState,
    head: &'a str,
}

/// Append an automatic event when the pull request's observed state changes.
fn record_transition_event(
    scribe: &Scribe,
    store: &mut Store,
    summaries: &[PullSummary],
    transition: PullTransition<'_>,
) -> Result<(), crate::ledger::LedgerError> {
    let state = transition.state.as_str();
    let settled = matches!(transition.state, PullState::Merged | PullState::Closed);
    let state_changed = store.pull_state(scribe.repo(), transition.number) != Some(state);
    // Forge state repeats for settled pulls, but every advanced classification
    // names a new head and is therefore a distinct event.
    if (!settled || state_changed)
        && let Some(text) = transition_text(transition.number, transition.state, transition.head)
    {
        let subject = summaries
            .iter()
            .find(|summary| summary.number == transition.number)
            .map(|summary| summary.head_ref_name.clone());
        let pr = subject
            .as_deref()
            .map(|branch| BranchTarget::new(scribe.repo().to_owned(), BranchName::new(branch)));
        scribe.event(
            subject.as_deref(),
            text,
            pr.and_then(|target| store.tracked_pull(&target)),
        )?;
    }
    store.record_pull_state(scribe.repo(), transition.number, state);
    Ok(())
}

struct TrackingInput<'a, 'snapshot> {
    tracked: BTreeMap<u64, String>,
    seen: &'a BTreeMap<String, String>,
    heads: &'a BTreeMap<u64, String>,
    summaries: &'a [PullSummary],
    snapshot: Option<&'a crate::snapshot::ForgeSnapshot<'snapshot>>,
    scribe: &'a Scribe,
    store: &'a mut Store,
}

fn record_tracked_pulls(
    input: TrackingInput<'_, '_>,
    report: &mut Report,
) -> Result<(), crate::ledger::LedgerError> {
    for (number, label) in input.tracked {
        let fact = input.snapshot.and_then(|snapshot| snapshot.fact(number));
        if input.snapshot.is_some() && fact.is_none() {
            report.problems.push(format!(
                "state of #{number} unavailable: the forge did not report it"
            ));
            continue;
        }
        let current = current_pull_head(input.heads, fact, number).unwrap_or_default();

        if current.is_empty() {
            // Neither the pull refs nor the live forge fact knew this head. Reporting
            // it as moved would be a fabrication, and because the empty head is
            // never recorded, the false "advanced" would repeat on every run.
            report
                .problems
                .push(format!("could not determine the head of #{number}"));
            continue;
        }

        let state = fact.map_or_else(|| "OPEN".to_owned(), |fact| fact.pull.state.clone());
        let transition = PullTransition {
            number,
            state: classify_pull(
                input.seen.get(&number.to_string()).map(String::as_str),
                current,
                &state,
            ),
            head: current,
        };
        record_transition_event(input.scribe, input.store, input.summaries, transition)?;
        report.rows.push(Row {
            number,
            label,
            state: transition.state,
        });
        input
            .store
            .record_pull_head(input.scribe.repo(), number, current);

        if state == "OPEN"
            && let Some(newest) = fact.and_then(|fact| fact.newest_comment.as_ref())
        {
            // `gh` emits fixed-width RFC-3339 UTC timestamps, so lexical ordering is chronological.
            let previous = input.store.comment_mark(input.scribe.repo(), number);
            let is_first_observation = previous.is_none();
            let has_advanced = previous.is_some_and(|mark| newest.as_str() > mark);
            if has_advanced {
                report.notes.push(format!(
                    "#{number} has comment activity newer than the last sync"
                ));
            }
            if is_first_observation || has_advanced {
                input
                    .store
                    .record_comment_mark(input.scribe.repo(), number, newest);
            }
        }
    }
    Ok(())
}

fn pull_heads_or_problem(entry: &RepoEntry, report: &mut Report) -> BTreeMap<u64, String> {
    match pull_heads(&entry.path, entry.remote(Role::Upstream)) {
        Ok(found) => found,
        Err(error) => {
            report
                .problems
                .push(format!("could not read pull refs: {error}"));
            BTreeMap::new()
        }
    }
}

fn persist_snapshot(snapshot: Option<&crate::snapshot::ForgeSnapshot<'_>>, report: &mut Report) {
    if let Some(snapshot) = snapshot
        && let Err(error) = snapshot.persist(None)
    {
        report
            .problems
            .push(format!("forge cache not saved: {error}"));
    }
}

pub fn sync_repo(input: SyncInput<'_>) -> anyhow::Result<Report> {
    let SyncInput {
        entry,
        store,
        forge,
        scribe,
        cache,
    } = input;
    let name = scribe.repo();
    let mut report = Report {
        repo: name.to_string(),
        ..Report::default()
    };
    fetch_all(&entry.path)?;

    let seen = store.pull_heads(name);
    let foreign: BTreeSet<u64> = store.foreign_parent_numbers(name).into_iter().collect();
    let opened = if let Some(forge) = forge {
        match crate::snapshot::open(crate::snapshot::SnapshotConfig {
            forge,
            path: &entry.path,
            remotes: [entry.remote(Role::Origin), entry.remote(Role::Release)],
            cache_root: cache,
        }) {
            Ok(opened) => Some(opened),
            Err(error) => {
                report
                    .problems
                    .push(format!("pull request state unavailable: {error}"));
                return Ok(report);
            }
        }
    } else {
        report
            .notes
            .push("pull request state was not checked; branch columns are unknown".to_owned());
        None
    };
    let discovery = match opened.as_ref() {
        Some(opened) => match opened.discover() {
            Ok(discovery) => Some(discovery),
            Err(error) => {
                report
                    .problems
                    .push(format!("pull request state unavailable: {error}"));
                return Ok(report);
            }
        },
        None => None,
    };
    let ours = discovery
        .as_ref()
        .map_or_else(Vec::new, crate::snapshot::Discovery::ours);
    let tracked = tracked_pull_requests(&ours, &foreign, &seen);
    let surfaced: Vec<u64> = tracked.keys().copied().collect();
    let snapshot = match discovery {
        Some(discovery) => match discovery.complete(&surfaced) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                report
                    .problems
                    .push(format!("pull request state unavailable: {error}"));
                return Ok(report);
            }
        },
        None => None,
    };
    let heads = pull_heads_or_problem(entry, &mut report);
    let summaries: &[PullSummary] = snapshot
        .as_ref()
        .map_or(&[], crate::snapshot::ForgeSnapshot::rows);
    record_tracked_pulls(
        TrackingInput {
            tracked,
            seen: &seen,
            heads: &heads,
            summaries,
            snapshot: snapshot.as_ref(),
            scribe,
            store,
        },
        &mut report,
    )?;

    persist_snapshot(snapshot.as_ref(), &mut report);

    fetch_foreign(
        &entry.path,
        entry.remote(Role::Upstream),
        &foreign,
        &mut report,
    );

    report.rows.sort_by_key(|row| row.number);
    store.save()?;
    Ok(report)
}

pub fn render(report: &Report) -> String {
    let mut lines: Vec<String> = report
        .notes
        .iter()
        .map(|note| format!("! {note}"))
        .collect();
    if report.rows.is_empty() {
        lines.push(format!("{}: no tracked pull requests", report.repo));
        return lines.join("\n");
    }
    lines.push(format!(
        "{}: {} tracked pull request(s)",
        report.repo,
        report.rows.len()
    ));
    for row in &report.rows {
        lines.push(format!(
            "  #{:<6} {:<10} {}",
            row.number,
            row.state.to_string(),
            row.label
        ));
    }
    lines.join("\n")
}

/// A command that could not answer must not report success, or a script gating
/// on it sees green.
pub const fn exit_for(report: &Report) -> Exit {
    if report.problems.is_empty() {
        Exit::Ok
    } else {
        Exit::Incomplete
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    #[test]
    fn forge_state_wins_over_head_movement() {
        // Given: a pull request whose head moved AND which merged
        // Then: merged, because calling it advanced keeps us carrying landed work
        assert_eq!(classify_pull(Some("a"), "b", "MERGED"), PullState::Merged);
        assert_eq!(classify_pull(Some("a"), "b", "CLOSED"), PullState::Closed);
    }

    #[test]
    fn a_moved_head_on_an_open_pull_request_is_advanced() {
        assert_eq!(classify_pull(Some("a"), "b", "OPEN"), PullState::Advanced);
    }

    #[test]
    fn an_unmoved_head_is_unchanged() {
        assert_eq!(classify_pull(Some("a"), "a", "OPEN"), PullState::Unchanged);
    }

    #[test]
    fn a_first_sighting_is_new_rather_than_unchanged() {
        // Calling it unchanged made a first run against an empty state file read as
        // "nothing moved since last time", which is indistinguishable from a no-op.
        assert_eq!(classify_pull(None, "a", "OPEN"), PullState::New);
        assert_eq!(classify_pull(Some("a"), "a", "OPEN"), PullState::Unchanged);
        assert_eq!(classify_pull(Some("a"), "b", "OPEN"), PullState::Advanced);
        // Forge state still wins over first sighting.
        assert_eq!(classify_pull(None, "a", "MERGED"), PullState::Merged);
    }

    #[test]
    fn state_matching_is_case_insensitive() {
        assert_eq!(classify_pull(None, "a", "merged"), PullState::Merged);
    }

    #[test]
    fn unchanged_and_new_pulls_have_no_transition_event() {
        // A first sighting and no movement are not facts this run observed.
        assert_eq!(transition_text(12, PullState::Unchanged, "head-12"), None);
        assert_eq!(transition_text(13, PullState::New, "head-13"), None);
    }

    #[test]
    fn a_report_with_a_problem_does_not_exit_zero() {
        let blocked = Report {
            problems: vec!["forge unavailable".to_owned()],
            ..Report::default()
        };
        assert_eq!(exit_for(&blocked), Exit::Incomplete);
    }

    #[test]
    fn an_informational_note_alone_still_exits_zero() {
        // Notes describe; problems mean the command could not answer.
        let chatty = Report {
            notes: vec!["nothing tracked yet".to_owned()],
            ..Report::default()
        };
        assert_eq!(exit_for(&chatty), Exit::Ok);
    }
}

#[cfg(test)]
mod tracking_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::forge::{PullRequest, PullSummary};

    fn pr(number: u64, branch: &str) -> (BranchName, PullRequest) {
        (
            BranchName::new(branch),
            PullRequest {
                number,
                head_ref_name: branch.to_owned(),
                head_ref_oid: "aaaa".to_owned(),
                ..PullRequest::default()
            },
        )
    }

    #[test]
    fn every_listed_pull_request_is_tracked_even_with_no_local_bookmark() {
        // Scoping now happens upstream, by head repository, so everything reaching
        // here is ours. It used to additionally require a local bookmark, which hid
        // any branch we had pushed but did not have checked out in this clone: the
        // The forge reported 13 open pull requests for a real repository and knives reported
        // 12, and the missing one was the single most actionable of them.
        let pull_requests: Vec<PullSummary> =
            [pr(1, "feat/checked-out"), pr(2, "feat/pushed-only")]
                .map(|(_, pull_request)| PullSummary::of(&pull_request))
                .into();
        let foreign: BTreeSet<u64> = BTreeSet::new();

        let tracked = tracked_pull_requests(&pull_requests, &foreign, &BTreeMap::new());

        let mut numbers: Vec<u64> = tracked.keys().copied().collect();
        numbers.sort_unstable();
        assert_eq!(numbers, vec![1, 2], "tracked the wrong set: {tracked:?}");
    }

    #[test]
    fn a_previously_tracked_number_stays_tracked_after_it_closes() {
        // It is absent from the list, and telling merged from closed is exactly
        // what the next step needs to do.
        let seen: BTreeMap<String, String> =
            std::iter::once(("99".to_owned(), "aaaa".to_owned())).collect();
        let tracked = tracked_pull_requests(&[], &BTreeSet::new(), &seen);
        assert!(tracked.contains_key(&99));
    }

    #[test]
    fn a_foreign_parent_is_tracked_without_a_listed_pull_request() {
        let foreign: BTreeSet<u64> = std::iter::once(4677).collect();
        let tracked = tracked_pull_requests(&[], &foreign, &BTreeMap::new());
        assert!(tracked[&4677].contains("foreign"));
    }
}

#[cfg(test)]
mod comment_activity_tests {
    use super::*;
    use crate::forge::{
        Forge, ForgeError, PullFacts, PullRequest, PullSummary, RepoIdentity, SweepEntry, SweepPage,
    };
    use crate::ids::RepoName;
    use crate::store::Store;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn pr(number: u64, branch: &str) -> (BranchName, PullRequest) {
        (
            BranchName::new(branch),
            PullRequest {
                number,
                head_ref_name: branch.to_owned(),
                head_ref_oid: "aaaa".to_owned(),
                ..PullRequest::default()
            },
        )
    }

    #[derive(Debug)]
    struct ErroringForge {
        pull_requests: Vec<PullRequest>,
        newest_comments: BTreeMap<u64, String>,
        error_on_comment: Option<u64>,
    }

    impl Forge for ErroringForge {
        fn repo_identity(&self, _repo: &Path) -> Result<RepoIdentity, ForgeError> {
            Ok(RepoIdentity {
                name_with_owner: "fake-owner/fake-repo".to_owned(),
                id: "FAKEID".to_owned(),
            })
        }

        fn list_pull_requests(
            &self,
            _repo: &Path,
            _authors: &[String],
        ) -> Result<Vec<PullSummary>, ForgeError> {
            Ok(self.pull_requests.iter().map(PullSummary::of).collect())
        }

        fn sweep(&self, _repo: &Path, _target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
            let mut entries = self
                .pull_requests
                .iter()
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
            _repo: &Path,
            _target: &RepoIdentity,
            numbers: &[u64],
        ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
            if self
                .error_on_comment
                .is_some_and(|number| numbers.contains(&number))
            {
                return Err(ForgeError::Query {
                    detail: "comment fetch failed".to_owned(),
                });
            }
            Ok(numbers
                .iter()
                .filter_map(|number| {
                    self.pull_requests
                        .iter()
                        .find(|pull| pull.number == *number)
                        .map(|pull| {
                            (
                                *number,
                                PullFacts {
                                    pull: pull.clone(),
                                    details: crate::forge::PullDetails::default(),
                                    newest_comment: self.newest_comments.get(number).cloned(),
                                },
                            )
                        })
                })
                .collect())
        }
    }

    struct PullListUnavailable;

    impl Forge for PullListUnavailable {
        fn repo_identity(&self, _repo: &Path) -> Result<RepoIdentity, ForgeError> {
            Ok(RepoIdentity {
                name_with_owner: "fake-owner/fake-repo".to_owned(),
                id: "FAKEID".to_owned(),
            })
        }

        fn list_pull_requests(
            &self,
            _repo: &Path,
            _authors: &[String],
        ) -> Result<Vec<PullSummary>, ForgeError> {
            Err(ForgeError::Command {
                command: "gh pr list".to_owned(),
                dir: "/repo".to_owned(),
                code: 1,
                stderr: "unavailable".to_owned(),
            })
        }

        fn sweep(&self, _repo: &Path, _target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
            Err(ForgeError::Command {
                command: "gh pr list".to_owned(),
                dir: "/repo".to_owned(),
                code: 1,
                stderr: "unavailable".to_owned(),
            })
        }

        fn pull_facts(
            &self,
            _repo: &Path,
            _target: &RepoIdentity,
            _numbers: &[u64],
        ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
            Ok(BTreeMap::new())
        }
    }

    /// Spawns `jj` (and sync itself spawns more), so every test using this
    /// fixture holds the environment lock for its whole body: a spawn racing a
    /// test that legally mutates PATH fails here, looking like a real flake.
    fn local_entry(temp: &TempDir) -> crate::config::RepoEntry {
        let work = temp.path().join("work");
        let origin = temp.path().join("origin");
        let jj = || {
            let mut cmd = Command::new("jj");
            cmd.env("JJ_CONFIG", "/dev/null")
                .env("JJ_USER", "Knives Lab")
                .env("JJ_EMAIL", "knives-lab@example.test");
            cmd
        };
        assert!(
            jj().args(["git", "init", "--colocate"])
                .arg(&work)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            jj().args(["git", "init", "--colocate"])
                .arg(&origin)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            jj().args(["git", "remote", "add", "origin"])
                .arg(&origin)
                .current_dir(&work)
                .status()
                .unwrap()
                .success()
        );
        crate::config::RepoEntry {
            path: work,
            upstream: origin.to_string_lossy().into_owned(),
            origin: origin.to_string_lossy().into_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        }
    }

    /// A scribe writing into the fixture's own directory. Every test that calls
    /// `sync_repo` needs one, and none of them may reach the real config home.
    fn test_scribe(temp: &TempDir, name: &RepoName) -> crate::ledger::Scribe {
        crate::ledger::Scribe::new(
            crate::ledger::Ledger::at(temp.path().join("ledger")),
            name.clone(),
            temp.path().to_owned(),
            "a-test".to_owned(),
        )
    }

    #[test]
    fn pull_request_list_failure_is_incomplete_not_informational() {
        let _lock = crate::config::test_support::environment_lock();
        let temp = TempDir::new().unwrap();
        let entry = local_entry(&temp);
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();

        let report = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&PullListUnavailable),
            scribe: &test_scribe(&temp, &RepoName::new("test-repo")),
            cache: None,
        })
        .unwrap();

        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("pull request state unavailable")),
            "was: {report:?}"
        );
        assert!(report.notes.is_empty(), "was: {report:?}");
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn pull_ref_failure_is_incomplete_not_informational() {
        let _lock = crate::config::test_support::environment_lock();
        let temp = TempDir::new().unwrap();
        let mut entry = local_entry(&temp);
        entry.upstream = temp.path().join("missing-upstream").display().to_string();
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();
        let forge = ErroringForge {
            pull_requests: Vec::new(),
            newest_comments: BTreeMap::new(),
            error_on_comment: None,
        };

        let report = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&forge),
            scribe: &test_scribe(&temp, &RepoName::new("test-repo")),
            cache: None,
        })
        .unwrap();

        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("could not read pull refs")),
            "was: {report:?}"
        );
        assert!(report.notes.is_empty(), "was: {report:?}");
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn no_github_skips_forge_queries_and_reports_the_unknown_state() {
        let _lock = crate::config::test_support::environment_lock();
        let temp = TempDir::new().unwrap();
        let entry = local_entry(&temp);
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();

        let report = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: None,
            scribe: &test_scribe(&temp, &RepoName::new("test-repo")),
            cache: None,
        })
        .unwrap();

        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("pull request state was not checked")),
            "was: {report:?}"
        );
        assert!(report.problems.is_empty(), "was: {report:?}");
    }

    #[test]
    fn comment_activity_reports_once_and_mark_persists() {
        let _lock = crate::config::test_support::environment_lock();
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);
        let scribe = test_scribe(&temp, &repo_name);

        // First sync: PR #42 with comment activity
        let (_branch, pull_request) = pr(42, "feat/alpha");
        let forge_first = ErroringForge {
            pull_requests: vec![pull_request],
            newest_comments: BTreeMap::from([(42, "2026-07-30T10:00:00Z".to_owned())]),
            error_on_comment: None,
        };

        let mut store = Store::open_for_update(store_path.clone()).unwrap();
        store.record_comment_mark(&repo_name, 42, "2026-07-29T10:00:00Z");
        let report1 = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&forge_first),
            scribe: &scribe,
            cache: None,
        })
        .unwrap();
        store.save().unwrap();

        // Verify: comment activity note appears with exact message
        assert!(
            report1
                .notes
                .iter()
                .any(|n| n.contains("42") && n.contains("comment activity")),
            "first sync should report comment activity: {report1:?}"
        );
        // Verify: mark was recorded in store
        assert_eq!(
            store.comment_mark(&repo_name, 42),
            Some("2026-07-30T10:00:00Z"),
            "comment mark not recorded in store"
        );

        // Second sync: same comment timestamp, no new activity
        let (_branch, pull_request) = pr(42, "feat/alpha");
        let forge_second = ErroringForge {
            pull_requests: vec![pull_request],
            newest_comments: BTreeMap::from([(42, "2026-07-29T10:00:00Z".to_owned())]),
            error_on_comment: None,
        };

        let mut store = Store::open(store_path).unwrap();
        let report2 = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&forge_second),
            scribe: &scribe,
            cache: None,
        })
        .unwrap();

        // Verify: no comment activity note on second run (mark unchanged)
        assert!(
            !report2.notes.iter().any(|n| n.contains("comment activity")),
            "second sync should not report unchanged comment: {report2:?}"
        );
        assert_eq!(
            store.comment_mark(&repo_name, 42),
            Some("2026-07-30T10:00:00Z")
        );
    }

    #[test]
    fn comment_batch_failure_is_incomplete_and_does_not_classify_rows() {
        let _lock = crate::config::test_support::environment_lock();
        let temp = TempDir::new().unwrap();
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);
        let (_branch, pull_request) = pr(42, "feat/alpha");
        let forge = ErroringForge {
            pull_requests: vec![pull_request],
            newest_comments: BTreeMap::new(),
            error_on_comment: Some(42),
        };
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();
        let report = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&forge),
            scribe: &test_scribe(&temp, &repo_name),
            cache: None,
        })
        .unwrap();

        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("pull request state unavailable")),
            "batch failure was not reported: {report:?}"
        );
        assert!(
            report.rows.is_empty(),
            "batch failure classified rows: {report:?}"
        );
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn first_sync_records_comment_activity_without_a_note() {
        let _lock = crate::config::test_support::environment_lock();
        // Given: a previously unseen open pull request with an existing comment
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);
        let (_branch, pull_request) = pr(42, "feat/alpha");
        let forge = ErroringForge {
            pull_requests: vec![pull_request],
            newest_comments: BTreeMap::from([(42, "2026-07-30T10:00:00Z".to_owned())]),
            error_on_comment: None,
        };
        let mut store = Store::open_for_update(store_path).unwrap();

        // When: the first sync observes the historical comment
        let report = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&forge),
            scribe: &test_scribe(&temp, &repo_name),
            cache: None,
        })
        .unwrap();

        // Then: its timestamp is remembered without announcing old activity
        assert_eq!(
            store.comment_mark(&repo_name, 42),
            Some("2026-07-30T10:00:00Z")
        );
        assert!(
            !report
                .notes
                .iter()
                .any(|note| note.contains("comment activity"))
        );
    }

    #[test]
    fn a_closed_pull_request_does_not_record_comment_activity() {
        let _lock = crate::config::test_support::environment_lock();
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);
        let (_branch, mut pull_request) = pr(42, "feat/alpha");
        pull_request.state = "CLOSED".to_owned();
        let forge = ErroringForge {
            pull_requests: vec![pull_request],
            newest_comments: BTreeMap::from([(42, "2026-07-30T10:00:00Z".to_owned())]),
            error_on_comment: None,
        };
        let mut store = Store::open_for_update(store_path).unwrap();

        let report = sync_repo(SyncInput {
            entry: &entry,
            store: &mut store,
            forge: Some(&forge),
            scribe: &test_scribe(&temp, &repo_name),
            cache: None,
        })
        .unwrap();

        assert_eq!(
            report.rows.first().map(|row| &row.state),
            Some(&PullState::Closed)
        );
        assert_eq!(store.comment_mark(&repo_name, 42), None);
        assert!(
            !report
                .notes
                .iter()
                .any(|note| note.contains("comment activity"))
        );
    }
}
