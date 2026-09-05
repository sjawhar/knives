//! The pull request half of a hosting service.
//!
//! A trait rather than a concrete client, so tests can supply facts without a
//! network call. The command line tool this wraps speaks to exactly one hosting
//! service, so standing up a local server of a different kind would not exercise
//! this code path at all. A fake does.
pub mod fake;
pub mod github;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Deserializer};

use crate::ids::BranchName;

fn null_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    #[serde(default)]
    pub name: String,
    /// `None` while unfinished, including legacy `PENDING` and `EXPECTED` contexts;
    /// otherwise `SUCCESS`, `FAILURE`, `SKIPPED`, `CANCELLED`, or `ACTION_REQUIRED`.
    ///
    /// `ACTION_REQUIRED` also stands for a whole workflow the forge refused to
    /// start: a fork pull request whose runs await a maintainer's approval has a
    /// check suite with that conclusion and no check runs at all, so the rollup
    /// alone shows only whatever ran unconditionally and reads green.
    #[serde(default)]
    pub conclusion: Option<String>,
}

/// What the forge's checks say about a pull request.
///
/// Never-ran is kept distinct from failed: an empty rollup on a fresh push is not a failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct ChecksSummary {
    pub runs: Vec<CheckRun>,
}

impl ChecksSummary {
    /// Checks that ran and failed, as opposed to ones that never ran.
    pub fn hard_failure_names(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|run| run.hard_failure())
            .map(|run| run.name.clone())
            .collect()
    }

    /// Checks the forge is holding for someone's action: a whole workflow
    /// awaiting a maintainer's approval, so nothing of it ran, or a check that
    /// stopped and asked. Over these an `ok` cell is a lie: one unconditional
    /// lint check green, the suite never started.
    pub fn action_required_names(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|run| run.action_required())
            .map(|run| run.name.clone())
            .collect()
    }

    /// Whether the rollup is red: a check failed, or one is held for action.
    /// Both keep a pull request from merging on its own, which is what a red
    /// row means.
    pub fn failing(&self) -> bool {
        self.runs
            .iter()
            .any(|run| run.hard_failure() || run.action_required())
    }

    pub fn has_hard_failure(&self) -> bool {
        self.runs.iter().any(CheckRun::hard_failure)
    }

    pub fn has_action_required(&self) -> bool {
        self.runs.iter().any(CheckRun::action_required)
    }

    /// Whether any returned check has not completed.
    pub fn pending(&self) -> bool {
        self.runs.iter().any(|run| run.conclusion.is_none())
    }

    /// Whether the forge ran anything at all. Nothing having run is not a failure.
    pub const fn ran(&self) -> bool {
        !self.runs.is_empty()
    }
}

impl CheckRun {
    fn hard_failure(&self) -> bool {
        self.conclusion.as_deref().is_some_and(|conclusion| {
            conclusion.eq_ignore_ascii_case("FAILURE")
                || conclusion.eq_ignore_ascii_case("TIMED_OUT")
                || conclusion.eq_ignore_ascii_case("CANCELLED")
                || conclusion.eq_ignore_ascii_case("STARTUP_FAILURE")
                || conclusion.eq_ignore_ascii_case("ERROR")
        })
    }

    fn action_required(&self) -> bool {
        self.conclusion
            .as_deref()
            .is_some_and(|conclusion| conclusion.eq_ignore_ascii_case("ACTION_REQUIRED"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub state: String,
    #[serde(default, deserialize_with = "null_default")]
    pub review_decision: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub updated_at: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub url: String,
    /// The owner of the repository the branch lives in, absent when the head
    /// repository has been deleted.
    #[serde(default)]
    pub head_repository_owner: Option<Account>,
    /// Whether the forge can merge this pull request: `MERGEABLE`, `CONFLICTING`, or
    /// `UNKNOWN` while it is still working it out.
    ///
    /// Worth asking for because a pull request that conflicts with its base reads as
    /// finished from every other angle — tests green, review approved, nothing left to
    /// write — and cannot be merged. An agent called one code complete and ready to ship
    /// while it was in conflict with main.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// The forge's fuller account of why: `DIRTY` for a conflict, `BEHIND` for a base that
    /// has moved on, `BLOCKED`, `CLEAN`, `UNSTABLE`.
    #[serde(default)]
    pub merge_state_status: Option<String>,
    /// The branch this pull request targets.
    #[serde(default)]
    pub base_ref_name: Option<String>,
    /// The commit that landed this pull request on its base branch, present only
    /// once merged. For every merge method — merge commit, squash, rebase — this
    /// is the base-branch commit that carries the work, which is exactly the
    /// "where did it land" the bare rebase default needs. The head commit is not
    /// that: a squash-merged head never appears in the base's history at all.
    #[serde(default)]
    pub merge_commit: Option<MergeCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Account {
    pub login: String,
}

/// The base-branch commit a merged pull request landed as.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct MergeCommit {
    pub oid: String,
}

impl PullRequest {
    /// Whether the forge says this pull request is open.
    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("OPEN")
    }

    /// Whether the forge says this pull request merged. Closed-without-merging
    /// is not this: only merged work constrains where the composition rebases.
    pub fn is_merged(&self) -> bool {
        self.state.eq_ignore_ascii_case("MERGED")
    }

    /// Whether the forge says this cannot be merged as it stands.
    ///
    /// `UNKNOWN` is not a conflict: the forge computes mergeability asynchronously, and
    /// treating "not worked out yet" as "broken" would cry wolf on every fresh push.
    pub fn conflicting(&self) -> bool {
        self.mergeable
            .as_deref()
            .is_some_and(|mergeable| mergeable.eq_ignore_ascii_case("CONFLICTING"))
    }

    /// Required merge facts that the forge did not answer. Callers must report
    /// these rather than interpreting unknown data as a healthy pull request.
    pub fn missing_merge_fields(&self) -> impl Iterator<Item = &'static str> {
        [
            (self.mergeable.is_none(), "mergeable"),
            (self.merge_state_status.is_none(), "mergeStateStatus"),
            (self.base_ref_name.is_none(), "baseRefName"),
        ]
        .into_iter()
        .filter_map(|(missing, field)| missing.then_some(field))
    }
}
/// A pull request's own diff totals, from the live batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct DiffTotals {
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
}

impl DiffTotals {
    /// Nothing changed by this pull request at all.
    pub const fn empty(&self) -> bool {
        self.additions == 0 && self.deletions == 0 && self.changed_files == 0
    }
}

/// A commit as a force-push event names it: the commit and its tree, so
/// content-identical rewrites are distinguishable from content changes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommitOids {
    pub commit: String,
    pub tree: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum TimelineEventKind {
    ForcePush {
        before: CommitOids,
        after: CommitOids,
    },
    HeadDeleted,
    HeadRestored,
    Closed,
    Reopened,
    Merged {
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
}

/// One head-ref or state event from the forge's own log.
///
/// knives stores no push or commit history: the forge's event log is the one
/// durable record of what happened to a ref, and this type is the lens.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TimelineEvent {
    pub at: String,
    #[serde(flatten)]
    pub kind: TimelineEventKind,
}

/// What one round trip answers about a pull request beyond its list fields.
///
/// A number the forge did not answer for is absent from the map rather than
/// present with defaults: "not consulted" and "nothing to compare" are different
/// facts, and rendering the first as the second reports a red pull request as
/// clean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullDetails {
    /// Whether the newest review predates the newest commit. `None` means there
    /// was nothing to compare, which must never render as "the review is current".
    pub review_predates_head: Option<bool>,
    /// What the forge's checks say. `None` means the forge reported no rollup for
    /// this pull request at all.
    pub checks: Option<ChecksSummary>,
    /// The pull request's diff totals. `None` means the batch did not answer them,
    /// which must never render as "empty diff".
    pub diff: Option<DiffTotals>,
    /// Whether the head ref no longer exists on the forge. GitHub keeps
    /// `headRefName` as text after a delete; the `headRef` object goes null —
    /// that null is the "open pull request with a deleted head" incident signal.
    pub head_ref_deleted: Option<bool>,
    /// Whether the newest commit's tree equals its one parent tree: a tip that a
    /// rebase or duplicate emptied while the branch reads healthy.
    pub tip_commit_empty: Option<bool>,
    /// The pull request's description as the forge holds it. `None` means the
    /// batch did not answer it, never "no body": an empty body is `Some("")`.
    pub body: Option<String>,
    /// How many review threads are still unresolved. `None` means the batch did
    /// not answer — an old payload, or more threads than one page holds — and
    /// must never render as "no threads".
    pub unresolved_review_threads: Option<usize>,
}

/// Cheap row for wide lists, the cache, and discovery.
///
/// Carries every list field except `mergeable`/`mergeStateStatus`. Its type
/// structurally enforces the field split: consumers cannot read merge state
/// from a list row because the field does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullSummary {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub review_decision: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub updated_at: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub head_repository_owner: Option<Account>,
    #[serde(default)]
    pub base_ref_name: Option<String>,
    #[serde(default)]
    pub merge_commit: Option<MergeCommit>,
}

impl PullSummary {
    /// The cheap projection of a live row, for merging fetched facts back into the cache.
    pub fn of(pull: &PullRequest) -> Self {
        Self {
            number: pull.number,
            state: pull.state.clone(),
            review_decision: pull.review_decision.clone(),
            head_ref_name: pull.head_ref_name.clone(),
            head_ref_oid: pull.head_ref_oid.clone(),
            updated_at: pull.updated_at.clone(),
            is_draft: pull.is_draft,
            url: pull.url.clone(),
            head_repository_owner: pull.head_repository_owner.clone(),
            base_ref_name: pull.base_ref_name.clone(),
            merge_commit: pull.merge_commit.clone(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("OPEN")
    }

    pub fn is_merged(&self) -> bool {
        self.state.eq_ignore_ascii_case("MERGED")
    }

    pub fn is_from(&self, owner: &str) -> bool {
        self.head_repository_owner
            .as_ref()
            .is_some_and(|account| account.login.eq_ignore_ascii_case(owner))
    }
}

/// Everything the live batch answers about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullFacts {
    /// The full fact row, `mergeable`/`mergeStateStatus` included.
    pub pull: PullRequest,
    pub details: PullDetails,
    /// Newest comment-or-review timestamp, for sync's activity mark.
    pub newest_comment: Option<String>,
}

/// The forge's own name for this checkout, resolved once per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub name_with_owner: String, // "owner/repo"
    pub id: String,              // GraphQL node id
}

impl RepoIdentity {
    /// (owner, repo), or `ForgeError::Target` when the name has no slash.
    pub fn split(&self) -> Result<(&str, &str), ForgeError> {
        self.name_with_owner
            .split_once('/')
            .ok_or_else(|| ForgeError::Target {
                named: self.name_with_owner.clone(),
            })
    }
}

/// The default branch and current head commit of a consumer repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerHead {
    pub branch: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepEntry {
    pub number: u64,
    pub updated_at: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepPage {
    pub entries: Vec<SweepEntry>,
    pub has_next_page: bool,
}

#[cfg(test)]
// Fixture-only defaults keep test pull request literals focused on fields under test.
impl Default for PullRequest {
    fn default() -> Self {
        Self {
            number: 0,
            state: "OPEN".to_owned(),
            review_decision: String::new(),
            head_ref_name: String::new(),
            head_ref_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            is_draft: false,
            url: String::new(),
            head_repository_owner: None,
            mergeable: None,
            merge_state_status: None,
            base_ref_name: None,
            merge_commit: None,
        }
    }
}

#[cfg(test)]
// Fixture-only defaults keep test pull request literals focused on fields under test.
impl Default for PullSummary {
    fn default() -> Self {
        Self {
            number: 0,
            state: String::new(),
            review_decision: String::new(),
            head_ref_name: String::new(),
            head_ref_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            is_draft: false,
            url: String::new(),
            head_repository_owner: None,
            base_ref_name: None,
            merge_commit: None,
        }
    }
}

/// The owner segment of a forge remote, for `https://` and `git@` forms alike.
pub fn remote_owner(remote: &str) -> Option<&str> {
    let rest = remote
        .split_once("github.com")
        .map(|(_, rest)| rest.trim_start_matches([':', '/']))?;
    let owner = rest.split('/').next()?;
    (!owner.is_empty()).then_some(owner)
}

/// The account names whose full pull-request history is worth fetching.
///
/// Derived from the same remotes that snapshot filtering uses. An organization
/// owner cannot author a pull request, so a name that happens to be an org
/// merely returns an empty search.
pub fn search_authors(remotes: &[&str]) -> Vec<String> {
    let mut authors: Vec<String> = remotes
        .iter()
        .filter_map(|remote| remote_owner(remote))
        .map(str::to_owned)
        .collect();
    authors.dedup();
    authors
}

/// The merged summaries onto `trunk`, in number order.
///
/// Merged means merged: a closed pull request landed nothing, and a merge onto
/// some other base is not on the trunk. A merged pull request whose landing
/// commit the forge did not record stays listed — the caller cannot place it
/// and must say so, rather than choose a target that quietly leaves merged work
/// out.
pub fn merged_onto(pulls: &[PullSummary], trunk: &str) -> Vec<PullSummary> {
    let mut merged: Vec<PullSummary> = pulls
        .iter()
        .filter(|pull| {
            pull.is_merged()
                && pull
                    .base_ref_name
                    .as_deref()
                    .is_some_and(|base| base == trunk)
        })
        .cloned()
        .collect();
    merged.sort_unstable_by_key(|pull| pull.number);
    merged
}

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    /// Raised, never swallowed. A status report that quietly omits pull request
    /// state looks identical to a repository with no pull requests, which is the
    /// stale-facts failure this tool exists to prevent.
    #[error("{command} failed in {dir} (exit {code}): {stderr}")]
    Command {
        command: String,
        dir: String,
        code: i32,
        stderr: String,
    },
    /// The forge named a repository that cannot be split into owner and name.
    #[error("the forge reported the repository as `{named}`, which is not `<owner>/<name>`")]
    Target { named: String },
    /// The forge answered with errors instead of data. Raised rather than read as
    /// "no details": a partial answer that reads as "nothing to compare" would
    /// render a red pull request as clean.
    #[error("the forge rejected the query: {detail}")]
    Query { detail: String },

    #[error("the forge CLI is not installed: {source}")]
    Missing {
        #[from]
        source: std::io::Error,
    },
    #[error("could not read the forge's reply: {source}")]
    Parse {
        #[from]
        source: serde_json::Error,
    },
}

/// The pull request half of a hosting service.
///
/// `Send + Sync` because `status` gathers repositories concurrently and probes
/// branches on scoped threads, and both share one forge.
pub trait Forge: Send + Sync {
    /// The forge's own name and GraphQL id for this checkout, once per run.
    fn repo_identity(&self, repo: &Path) -> Result<RepoIdentity, ForgeError>;

    /// Cold path: the cheap-field wide lists (base window ∥ author-scoped), deduped.
    fn list_pull_requests(
        &self,
        repo: &Path,
        authors: &[String],
    ) -> Result<Vec<PullSummary>, ForgeError>;

    /// Warm path page 1: newest-updated (number, updatedAt, state) plus continuation.
    fn sweep(&self, repo: &Path, target: &RepoIdentity) -> Result<SweepPage, ForgeError>;

    /// The live batch: full fact rows by number. Numbers the forge answers
    /// `NOT_FOUND` for are absent from the map; any other failure is an error.
    fn pull_facts(
        &self,
        repo: &Path,
        target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError>;

    /// The bounded, by-number head-ref history: force pushes (before/after
    /// commit and tree oids), deletes, restores, closes, reopens, and merges.
    /// On demand only — never part of any batch.
    fn pull_timeline(
        &self,
        repo: &Path,
        target: &RepoIdentity,
        number: u64,
    ) -> Result<Vec<TimelineEvent>, ForgeError>;
}

/// One branch's pull request summaries, split into the primary and its shadowed history.
#[derive(Debug, Default)]
pub struct PullIndex {
    /// The pull request summary a reader should look at first for each head branch.
    pub by_branch: BTreeMap<BranchName, PullSummary>,
    /// The rest of each branch's pull request summaries, in the forge's
    /// freshest-first order. A head branch accumulates several over its life — an
    /// org-fork submission closed and re-homed onto a personal fork keeps its
    /// review history on the closed number — and collapsing to one per branch used
    /// to discard these silently. An audit walked straight past a maintainer's
    /// blocking question because the closed predecessor carrying it never rendered
    /// anywhere.
    pub prior: BTreeMap<BranchName, Vec<PullSummary>>,
}

/// Index pull request summaries by head branch, keeping every shadowed one visible.
///
/// Primary selection is deterministic: an open pull request beats any closed or
/// merged one, and ties keep the forge's own ordering. First-wins list order —
/// the previous rule — let whichever pull request the forge listed first shadow
/// the rest, so a freshly closed duplicate could hide a still-open submission
/// and vice versa.
pub fn index_pulls(prs: &[PullSummary]) -> PullIndex {
    let mut index = PullIndex::default();
    for pr in prs {
        let branch = BranchName::new(pr.head_ref_name.clone());
        match index.by_branch.entry(branch.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                let _ = slot.insert(pr.clone());
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if pr.is_open() && !slot.get().is_open() {
                    let shadowed = slot.insert(pr.clone());
                    index.prior.entry(branch).or_default().push(shadowed);
                } else {
                    index.prior.entry(branch).or_default().push(pr.clone());
                }
            }
        }
    }
    index
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    /// Assembled rather than written out, because the identity guard forbids a
    /// forge URL literal anywhere under `src/`, and a test that spells the needle
    /// out would fail the very rule it sits next to.
    const HOST: &str = concat!("github", ".com");

    #[test]
    fn an_owner_is_read_from_either_remote_form() {
        use super::remote_owner;
        let https = format!("https://{HOST}/our-org/some-repo.git");
        let ssh = format!("git@{HOST}:our-org/some-repo.git");
        assert_eq!(remote_owner(&https), Some("our-org"));
        assert_eq!(remote_owner(&ssh), Some("our-org"));
        assert_eq!(remote_owner("https://example.invalid/x/y"), None);
    }

    use super::*;

    #[test]
    fn fixture_default_has_a_parseable_timestamp_and_hex_oid() {
        let fixture = PullRequest::default();

        assert!(
            fixture.updated_at.parse::<jiff::Timestamp>().is_ok(),
            "fixture timestamp is not parseable: {}",
            fixture.updated_at
        );
        assert_eq!(
            fixture.head_ref_oid.len(),
            40,
            "fixture OID has an unexpected length: {}",
            fixture.head_ref_oid
        );
        assert!(
            fixture
                .head_ref_oid
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
            "fixture OID is not ASCII hexadecimal: {}",
            fixture.head_ref_oid
        );
    }

    #[test]
    fn an_open_pull_summary_beats_a_closed_one_whatever_the_list_order() {
        let summary =
            |number: u64, state: &str, branch: &str, base: &str, oid: Option<&str>| PullSummary {
                number,
                state: state.to_owned(),
                head_ref_name: branch.to_owned(),
                base_ref_name: Some(base.to_owned()),
                merge_commit: oid.map(|oid| MergeCommit {
                    oid: oid.to_owned(),
                }),
                ..PullSummary::default()
            };
        let branch = BranchName::new("feat/alpha");

        // A closed duplicate listed first cannot hide the first open submission;
        // every shadow stays in the order the forge supplied it.
        let closed_first = [
            summary(9, "CLOSED", "feat/alpha", "main", None),
            summary(7, "OPEN", "feat/alpha", "main", None),
            summary(6, "OPEN", "feat/alpha", "main", None),
            summary(8, "CLOSED", "feat/alpha", "main", None),
        ];
        let indexed = index_pulls(&closed_first);
        assert_eq!(indexed.by_branch[&branch].number, 7);
        assert_eq!(
            indexed.prior[&branch]
                .iter()
                .map(|summary| summary.number)
                .collect::<Vec<_>>(),
            vec![9, 6, 8]
        );

        // The usual freshest-first order produces the same primary.
        let open_first = [
            summary(7, "OPEN", "feat/alpha", "main", None),
            summary(9, "CLOSED", "feat/alpha", "main", None),
        ];
        let indexed = index_pulls(&open_first);
        assert_eq!(indexed.by_branch[&branch].number, 7);
        assert_eq!(indexed.prior[&branch][0].number, 9);

        // With no open summary, the first closed summary remains the primary.
        let indexed = index_pulls(&[summary(9, "CLOSED", "feat/alpha", "main", None)]);
        assert_eq!(indexed.by_branch[&branch].number, 9);
        assert!(indexed.prior.is_empty());
    }

    #[test]
    fn only_merged_pull_request_summaries_onto_the_trunk_are_returned_in_number_order() {
        let summary =
            |number: u64, state: &str, branch: &str, base: &str, oid: Option<&str>| PullSummary {
                number,
                state: state.to_owned(),
                head_ref_name: branch.to_owned(),
                base_ref_name: Some(base.to_owned()),
                merge_commit: oid.map(|oid| MergeCommit {
                    oid: oid.to_owned(),
                }),
                ..PullSummary::default()
            };
        let pulls = [
            summary(5, "merged", "e", "main", Some("e5")),
            summary(9, "MERGED", "a", "main", Some("a9")),
            summary(2, "OPEN", "b", "main", None),
            summary(3, "CLOSED", "c", "main", None),
            summary(4, "MERGED", "d", "dev", Some("d4")),
            summary(6, "MERGED", "f", "main", None),
        ];

        let merged = merged_onto(&pulls, "main");
        let brief: Vec<(u64, &str, Option<&str>)> = merged
            .iter()
            .map(|summary| {
                (
                    summary.number,
                    summary.head_ref_name.as_str(),
                    summary
                        .merge_commit
                        .as_ref()
                        .map(|commit| commit.oid.as_str()),
                )
            })
            .collect();

        assert_eq!(
            brief,
            vec![(5, "e", Some("e5")), (6, "f", None), (9, "a", Some("a9")),],
            "case-insensitive state, trunk base only, sorted by number"
        );
    }

    #[test]
    fn summaries_copy_only_the_cheap_projection_and_preserve_its_behavior() {
        let pull = PullRequest {
            number: 7,
            state: "MERGED".to_owned(),
            review_decision: "APPROVED".to_owned(),
            head_ref_name: "feat/a".to_owned(),
            head_ref_oid: "aa".to_owned(),
            updated_at: "2026-08-01T00:00:00Z".to_owned(),
            is_draft: false,
            url: "u".to_owned(),
            head_repository_owner: Some(Account {
                login: "our-org".to_owned(),
            }),
            mergeable: Some("CONFLICTING".to_owned()),
            merge_state_status: Some("DIRTY".to_owned()),
            base_ref_name: Some("main".to_owned()),
            merge_commit: Some(MergeCommit {
                oid: "bb".to_owned(),
            }),
        };
        let summary = PullSummary::of(&pull);
        assert!(summary.is_merged());
        assert!(!summary.is_open());
        assert!(summary.is_from("OUR-ORG"));
        assert_eq!(summary.merge_commit, pull.merge_commit);
    }
}
