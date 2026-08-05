//! `knives sync`: fetch, then classify what happened to each tracked pull request.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use crate::cli::Exit;
use crate::config::{RepoEntry, Role};
use crate::forge::{Forge, PullRequest, ours_only};
use crate::ids::{BranchName, RepoName};
use crate::jj::{fetch_all, fetch_pull_ref, pull_heads};
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

impl fmt::Display for PullState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unchanged => "unchanged",
            Self::New => "new",
            Self::Advanced => "advanced",
            Self::Merged => "merged",
            Self::Closed => "closed",
        })
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
    pull_requests: &BTreeMap<BranchName, PullRequest>,
    foreign: &BTreeSet<u64>,
    seen: &BTreeMap<String, String>,
) -> BTreeMap<u64, String> {
    let mut tracked: BTreeMap<u64, String> = BTreeMap::new();
    for (branch, pr) in pull_requests {
        let _ = tracked.insert(pr.number, branch.to_string());
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

fn resolve_state(
    pull_requests: &BTreeMap<BranchName, PullRequest>,
    number: u64,
    forge: &dyn Forge,
    path: &Path,
) -> Result<String, crate::forge::ForgeError> {
    if let Some(pull_request) = pull_requests
        .values()
        .find(|pull_request| pull_request.number == number)
    {
        return Ok(pull_request.state.clone());
    }
    forge
        .pull_request_state(path, number)
        .map(|state| state.unwrap_or_else(|| "OPEN".to_owned()))
}

fn sync_pull_requests(
    forge: Option<&dyn Forge>,
    entry: &RepoEntry,
    report: &mut Report,
) -> Result<BTreeMap<BranchName, PullRequest>, ()> {
    let Some(forge) = forge else {
        report
            .notes
            .push("pull request state was not checked; branch columns are unknown".to_owned());
        return Ok(BTreeMap::new());
    };
    match forge.pull_requests(&entry.path) {
        Ok(found) => Ok(ours_only(
            found,
            &[entry.remote(Role::Origin), entry.remote(Role::Release)],
        )),
        Err(error) => {
            report
                .problems
                .push(format!("pull request state unavailable: {error}"));
            Err(())
        }
    }
}

pub fn sync_repo(
    name: &RepoName,
    entry: &RepoEntry,
    store: &mut Store,
    forge: Option<&dyn Forge>,
) -> anyhow::Result<Report> {
    let mut report = Report {
        repo: name.to_string(),
        ..Report::default()
    };
    fetch_all(&entry.path)?;

    let Ok(pull_requests) = sync_pull_requests(forge, entry, &mut report) else {
        return Ok(report);
    };

    let heads = match pull_heads(&entry.path, entry.remote(Role::Upstream)) {
        Ok(found) => found,
        Err(error) => {
            report
                .problems
                .push(format!("could not read pull refs: {error}"));
            BTreeMap::new()
        }
    };

    let seen = store.pull_heads(name);
    let foreign: BTreeSet<u64> = store.foreign_parent_numbers(name).into_iter().collect();

    // Tracked means ours: a pull request whose head is a branch we carry, plus
    // foreign ones we deliberately carry as release parents, plus anything we
    // tracked before. Every open pull request on the upstream is not our
    // business and buries the signal; on a real repository that was 10 rows
    // against 83.
    let tracked = tracked_pull_requests(&pull_requests, &foreign, &seen);

    for (number, label) in tracked {
        let current = heads
            .get(&number)
            .cloned()
            .or_else(|| {
                pull_requests
                    .values()
                    .find(|pr| pr.number == number)
                    .map(|pr| pr.head_ref_oid.clone())
            })
            .unwrap_or_default();

        // A tracked number absent from the pull request list is merged or closed, and
        // the list cannot say which. Resolve only those, so a run where
        // nothing vanished costs one query.
        if current.is_empty() {
            // Neither the pull refs nor the pull request list knew this head. Reporting
            // it as moved would be a fabrication, and because the empty head is
            // never recorded, the false "advanced" would repeat on every run.
            report
                .problems
                .push(format!("could not determine the head of #{number}"));
            continue;
        }

        let state = match forge {
            Some(forge) => match resolve_state(&pull_requests, number, forge, &entry.path) {
                Ok(state) => state,
                Err(error) => {
                    report
                        .problems
                        .push(format!("state of #{number} unavailable: {error}"));
                    continue;
                }
            },
            None => "OPEN".to_owned(),
        };

        report.rows.push(Row {
            number,
            label,
            state: classify_pull(
                seen.get(&number.to_string()).map(String::as_str),
                &current,
                &state,
            ),
        });
        if !current.is_empty() {
            store.record_pull_head(name, number, &current);
        }

        if state == "OPEN"
            && let Some(forge) = forge
        {
            // Check comment activity only while maintainers can still act on it.
            match forge.newest_comment(&entry.path, number) {
                Ok(Some(newest)) => {
                    let previous = store.comment_mark(name, number);
                    let is_first_observation = previous.is_none();
                    let has_advanced = previous.is_some_and(|mark| newest.as_str() > mark);
                    if has_advanced {
                        report.notes.push(format!(
                            "#{number} has comment activity newer than the last sync"
                        ));
                    }
                    if is_first_observation || has_advanced {
                        // `gh` emits fixed-width RFC-3339 UTC timestamps, so lexical ordering is chronological.
                        store.record_comment_mark(name, number, &newest);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    report
                        .problems
                        .push(format!("could not check comments on #{number}: {error}"));
                }
            }
        }
    }

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
    fn resolve_state_returns_listed_state_without_fake_fallback() {
        use crate::forge::FakeForge;

        let (branch, mut pull_request) = pr(99, "feat/merged");
        pull_request.state = "MERGED".to_owned();
        let pull_requests = BTreeMap::from([(branch, pull_request)]);
        let forge = FakeForge {
            // The fallback deliberately contradicts listed MERGED: ignoring it returns OPEN.
            vanished_states: BTreeMap::from([(99, "OPEN".to_owned())]),
            ..FakeForge::default()
        };

        assert_eq!(
            resolve_state(&pull_requests, 99, &forge, std::path::Path::new("/repo")).unwrap(),
            "MERGED"
        );
    }

    #[test]
    fn resolve_state_queries_fake_fallback_for_an_absent_number() {
        use crate::forge::FakeForge;

        let forge = FakeForge {
            vanished_states: BTreeMap::from([(100, "CLOSED".to_owned())]),
            ..FakeForge::default()
        };

        assert_eq!(
            resolve_state(&BTreeMap::new(), 100, &forge, std::path::Path::new("/repo")).unwrap(),
            "CLOSED"
        );
    }

    #[test]
    fn every_listed_pull_request_is_tracked_even_with_no_local_bookmark() {
        // Scoping now happens upstream, by head repository, so everything reaching
        // here is ours. It used to additionally require a local bookmark, which hid
        // any branch we had pushed but did not have checked out in this clone: the
        // The forge reported 13 open pull requests for a real repository and knives reported
        // 12, and the missing one was the single most actionable of them.
        let pull_requests: BTreeMap<_, _> = [pr(1, "feat/checked-out"), pr(2, "feat/pushed-only")]
            .into_iter()
            .collect();
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
        let tracked = tracked_pull_requests(&BTreeMap::new(), &BTreeSet::new(), &seen);
        assert!(tracked.contains_key(&99));
    }

    #[test]
    fn a_foreign_parent_is_tracked_without_a_listed_pull_request() {
        let foreign: BTreeSet<u64> = std::iter::once(4677).collect();
        let tracked = tracked_pull_requests(&BTreeMap::new(), &foreign, &BTreeMap::new());
        assert!(tracked[&4677].contains("foreign"));
    }
}

#[cfg(test)]
mod comment_activity_tests {
    use super::*;
    use crate::forge::{ChecksSummary, Forge, ForgeError};
    use crate::store::Store;
    use std::cell::RefCell;
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

    #[derive(Debug, Clone)]
    struct ErroringForge {
        pull_requests: BTreeMap<BranchName, PullRequest>,
        newest_comments: BTreeMap<u64, String>,
        error_on_comment: Option<u64>, // PR number that errors, None = no error
        comment_calls: RefCell<Vec<u64>>,
    }

    impl Forge for ErroringForge {
        fn pull_requests(
            &self,
            _repo: &Path,
        ) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
            Ok(self.pull_requests.clone())
        }

        fn review_predates_head(
            &self,
            _repo: &Path,
            _number: u64,
        ) -> Result<Option<bool>, ForgeError> {
            Ok(None)
        }

        fn checks(&self, _repo: &Path, _number: u64) -> Result<Option<ChecksSummary>, ForgeError> {
            Ok(None)
        }

        fn pull_request_state(
            &self,
            _repo: &Path,
            _number: u64,
        ) -> Result<Option<String>, ForgeError> {
            Ok(None)
        }

        fn newest_comment(&self, _repo: &Path, number: u64) -> Result<Option<String>, ForgeError> {
            self.comment_calls.borrow_mut().push(number);
            if self.error_on_comment == Some(number) {
                return Err(ForgeError::Command {
                    command: "gh pr view".to_owned(),
                    dir: "/repo".to_owned(),
                    code: 1,
                    stderr: "could not fetch comments".to_owned(),
                });
            }
            Ok(self.newest_comments.get(&number).cloned())
        }
    }

    struct PullListUnavailable;

    impl Forge for PullListUnavailable {
        fn pull_requests(
            &self,
            _repo: &Path,
        ) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
            Err(ForgeError::Command {
                command: "gh pr list".to_owned(),
                dir: "/repo".to_owned(),
                code: 1,
                stderr: "unavailable".to_owned(),
            })
        }

        fn review_predates_head(
            &self,
            _repo: &Path,
            _number: u64,
        ) -> Result<Option<bool>, ForgeError> {
            Ok(None)
        }

        fn checks(&self, _repo: &Path, _number: u64) -> Result<Option<ChecksSummary>, ForgeError> {
            Ok(None)
        }

        fn pull_request_state(
            &self,
            _repo: &Path,
            _number: u64,
        ) -> Result<Option<String>, ForgeError> {
            Ok(None)
        }

        fn newest_comment(&self, _repo: &Path, _number: u64) -> Result<Option<String>, ForgeError> {
            Ok(None)
        }
    }

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

    #[test]
    fn pull_request_list_failure_is_incomplete_not_informational() {
        let temp = TempDir::new().unwrap();
        let entry = local_entry(&temp);
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();

        let report = sync_repo(
            &RepoName::new("test-repo"),
            &entry,
            &mut store,
            Some(&PullListUnavailable),
        )
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
        let temp = TempDir::new().unwrap();
        let mut entry = local_entry(&temp);
        entry.upstream = temp.path().join("missing-upstream").display().to_string();
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();
        let forge = ErroringForge {
            pull_requests: BTreeMap::new(),
            newest_comments: BTreeMap::new(),
            error_on_comment: None,
            comment_calls: RefCell::new(Vec::new()),
        };

        let report = sync_repo(
            &RepoName::new("test-repo"),
            &entry,
            &mut store,
            Some(&forge),
        )
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
        let temp = TempDir::new().unwrap();
        let entry = local_entry(&temp);
        let mut store = Store::open_for_update(temp.path().join("state.json")).unwrap();

        let report = sync_repo(&RepoName::new("test-repo"), &entry, &mut store, None).unwrap();

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
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);

        // First sync: PR #42 with comment activity
        let (branch, pull_request) = pr(42, "feat/alpha");
        let forge_first = ErroringForge {
            pull_requests: BTreeMap::from([(branch, pull_request)]),
            newest_comments: BTreeMap::from([(42, "2026-07-30T10:00:00Z".to_owned())]),
            error_on_comment: None,
            comment_calls: RefCell::new(Vec::new()),
        };

        let mut store = Store::open_for_update(store_path.clone()).unwrap();
        store.record_comment_mark(&repo_name, 42, "2026-07-29T10:00:00Z");
        let report1 = sync_repo(&repo_name, &entry, &mut store, Some(&forge_first)).unwrap();
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
        let (branch, pull_request) = pr(42, "feat/alpha");
        let forge_second = ErroringForge {
            pull_requests: BTreeMap::from([(branch, pull_request)]),
            newest_comments: BTreeMap::from([(42, "2026-07-29T10:00:00Z".to_owned())]),
            error_on_comment: None,
            comment_calls: RefCell::new(Vec::new()),
        };

        let mut store = Store::open(store_path).unwrap();
        let report2 = sync_repo(&repo_name, &entry, &mut store, Some(&forge_second)).unwrap();

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
    fn newest_comment_error_goes_to_problems_not_notes() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);

        // Forge that fails on newest_comment for PR #42
        let (branch, pull_request) = pr(42, "feat/alpha");
        let forge = ErroringForge {
            pull_requests: BTreeMap::from([(branch, pull_request)]),
            newest_comments: BTreeMap::new(),
            error_on_comment: Some(42),
            comment_calls: RefCell::new(Vec::new()),
        };

        let mut store = Store::open_for_update(store_path).unwrap();
        let report = sync_repo(&repo_name, &entry, &mut store, Some(&forge)).unwrap();

        // Verify: error went to problems, not notes
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("42") && p.contains("comment")),
            "newest_comment error should be in problems: {report:?}"
        );
        assert!(
            !report.notes.iter().any(|n| n.contains("comment")),
            "newest_comment error should not be in notes: {report:?}"
        );
        // Verify: exit code reflects the problem
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn first_sync_records_comment_activity_without_a_note() {
        // Given: a previously unseen open pull request with an existing comment
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);
        let (branch, pull_request) = pr(42, "feat/alpha");
        let forge = ErroringForge {
            pull_requests: BTreeMap::from([(branch, pull_request)]),
            newest_comments: BTreeMap::from([(42, "2026-07-30T10:00:00Z".to_owned())]),
            error_on_comment: None,
            comment_calls: RefCell::new(Vec::new()),
        };
        let mut store = Store::open_for_update(store_path).unwrap();

        // When: the first sync observes the historical comment
        let report = sync_repo(&repo_name, &entry, &mut store, Some(&forge)).unwrap();

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
    fn a_closed_pull_request_skips_comment_activity_lookup() {
        // Given: a closed tracked pull request with a comment available from the forge
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("state.json");
        let repo_name = RepoName::new("test-repo");
        let entry = local_entry(&temp);
        let (branch, mut pull_request) = pr(42, "feat/alpha");
        pull_request.state = "CLOSED".to_owned();
        let forge = ErroringForge {
            pull_requests: BTreeMap::from([(branch, pull_request)]),
            newest_comments: BTreeMap::from([(42, "2026-07-30T10:00:00Z".to_owned())]),
            error_on_comment: None,
            comment_calls: RefCell::new(Vec::new()),
        };
        let mut store = Store::open_for_update(store_path).unwrap();

        // When: sync classifies the settled pull request
        let report = sync_repo(&repo_name, &entry, &mut store, Some(&forge)).unwrap();

        // Then: the settled pull request is reported but never queried for comments
        assert_eq!(
            report.rows.first().map(|row| &row.state),
            Some(&PullState::Closed)
        );
        assert!(forge.comment_calls.borrow().is_empty());
    }
}
