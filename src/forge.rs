//! The pull request half of a hosting service.
//!
//! A trait rather than a concrete client, so tests can supply facts without a
//! network call. The command line tool this wraps speaks to exactly one hosting
//! service, so standing up a local server of a different kind would not exercise
//! this code path at all. A fake does.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Deserializer};

use crate::ids::BranchName;

const PR_STATE: &str = "all";
// headRepositoryOwner is what makes a pull request ours or someone else's. Without
// it, ownership was inferred from the head branch name, so an outside contributor
// whose branch is called `main` was tracked as our work.
const PR_FIELDS: &str = "number,state,reviewDecision,headRefName,headRefOid,updatedAt,isDraft,url,\
     headRepositoryOwner,mergeable,mergeStateStatus,baseRefName,mergeCommit";
const PR_LIST_ARGS: [&str; 8] = [
    "pr", "list", "--state", PR_STATE, "--limit", "300", "--json", PR_FIELDS,
];

/// The arguments used to list pull requests from the forge.
pub const fn pull_request_list_args() -> &'static [&'static str; 8] {
    &PR_LIST_ARGS
}

/// The fields we ask the forge for.
///
/// Exposed so a test can check the type and the request have not drifted apart: a field
/// added to `PullRequest` but not here deserialises as its default forever, and the report
/// degrades with nothing failing.
pub const fn requested_fields() -> &'static str {
    PR_FIELDS
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    #[serde(default)]
    pub name: String,
    /// `SUCCESS`, `FAILURE`, `SKIPPED`, `CANCELLED`, or empty while still running.
    #[serde(default)]
    pub conclusion: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum CheckRunPayload {
    CheckRun {
        #[serde(default)]
        name: String,
        #[serde(default)]
        conclusion: String,
    },
    StatusContext {
        #[serde(default)]
        context: String,
        #[serde(default)]
        state: String,
    },
}

impl<'de> Deserialize<'de> for CheckRun {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // An unknown typename rejects the complete rollup, including any earlier known
        // failures. Reporting unavailable checks is preferable to silently calling a
        // known-red pull request clean when the forge adds an unrecognised variant.
        match CheckRunPayload::deserialize(deserializer)? {
            CheckRunPayload::CheckRun { name, conclusion } => Ok(Self { name, conclusion }),
            CheckRunPayload::StatusContext { context, state } => Ok(Self {
                name: context,
                conclusion: state,
            }),
        }
    }
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
    /// Checks the forge reported a failing conclusion for.
    pub fn failed_names(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|run| {
                run.conclusion.eq_ignore_ascii_case("FAILURE")
                    || run.conclusion.eq_ignore_ascii_case("TIMED_OUT")
                    || run.conclusion.eq_ignore_ascii_case("CANCELLED")
                    || run.conclusion.eq_ignore_ascii_case("STARTUP_FAILURE")
                    || run.conclusion.eq_ignore_ascii_case("ACTION_REQUIRED")
                    || run.conclusion.eq_ignore_ascii_case("ERROR")
            })
            .map(|run| run.name.clone())
            .collect()
    }

    pub fn failing(&self) -> bool {
        !self.failed_names().is_empty()
    }

    /// Whether the forge ran anything at all. Nothing having run is not a failure.
    pub const fn ran(&self) -> bool {
        !self.runs.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
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
    pub mergeable: String,
    /// The forge's fuller account of why: `DIRTY` for a conflict, `BEHIND` for a base that
    /// has moved on, `BLOCKED`, `CLEAN`, `UNSTABLE`.
    #[serde(default)]
    pub merge_state_status: String,
    /// The branch this pull request targets.
    #[serde(default)]
    pub base_ref_name: String,
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
        self.mergeable.eq_ignore_ascii_case("CONFLICTING")
    }

    /// Whether this pull request comes from `owner`'s copy of the repository.
    ///
    /// `None` for the owner means the head repository is gone, which cannot be
    /// ours, and an unknown owner must not be assumed to be ours: the whole point
    /// is to stop treating other people's branches as our work.
    pub fn is_from(&self, owner: &str) -> bool {
        self.head_repository_owner
            .as_ref()
            .is_some_and(|account| account.login.eq_ignore_ascii_case(owner))
    }
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
            mergeable: String::new(),
            merge_state_status: String::new(),
            base_ref_name: "main".to_owned(),
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

/// Keep only the pull requests that come from our own copy of the repository.
pub fn ours_only(
    pull_requests: BTreeMap<BranchName, PullRequest>,
    remotes: &[&str],
) -> BTreeMap<BranchName, PullRequest> {
    let owners: Vec<&str> = remotes
        .iter()
        .filter_map(|remote| remote_owner(remote))
        .collect();
    if owners.is_empty() {
        // A set of remotes we cannot parse is not a licence to claim everyone's work,
        // and not a reason to claim nobody's either: keep today's fail-open answer.
        return pull_requests;
    }
    pull_requests
        .into_iter()
        .filter(|(_, pr)| owners.iter().any(|owner| pr.is_from(owner)))
        .collect()
}

/// One of our pull requests the forge says merged onto the trunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedPull {
    pub number: u64,
    /// The head branch it was merged from: the member a release may still carry.
    pub branch: BranchName,
    /// The trunk commit it landed as, when the forge recorded one.
    pub oid: Option<String>,
}

/// Where each merged pull request landed on `trunk`, in number order.
///
/// Merged means merged: a closed pull request landed nothing, and a merge onto
/// some other base is not on the trunk. A merged pull request whose landing
/// commit the forge did not record stays listed with `None` — the caller cannot
/// place it and must say so, rather than choose a target that quietly leaves
/// merged work out.
pub fn merged_onto(
    pull_requests: &BTreeMap<BranchName, PullRequest>,
    trunk: &str,
) -> Vec<LandedPull> {
    let mut landed: Vec<LandedPull> = pull_requests
        .iter()
        .filter(|(_, pr)| pr.is_merged() && pr.base_ref_name == trunk)
        .map(|(branch, pr)| LandedPull {
            number: pr.number,
            branch: branch.clone(),
            oid: pr.merge_commit.as_ref().map(|merge| merge.oid.clone()),
        })
        .collect();
    landed.sort_unstable_by_key(|pull| pull.number);
    landed
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
    /// Pull requests in every state, indexed by head branch name.
    fn pull_requests(&self, repo: &Path) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError>;

    /// Review age and check state for many pull requests in one round trip.
    ///
    /// This replaces a per-pull-request pair — a review-timeline query and a check
    /// rollup query — each of which cost a process spawn plus an HTTPS round trip.
    /// A repository with nine open pull requests spent eighteen serial calls where
    /// one query now answers, and that was most of what made `status` slow. The
    /// rollup is asked for here rather than in the list query because there it
    /// exceeds the forge's GraphQL budget and fails the whole call.
    ///
    /// A number the forge does not answer for is absent from the map. Callers
    /// must keep that distinct from an empty answer.
    fn pull_details(
        &self,
        repo: &Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullDetails>, ForgeError>;

    /// The state of one pull request by number, whatever that state is.
    ///
    /// Resolving a tracked number absent from the pull request list is the only way to tell
    /// "merged" from "closed" from "we stopped tracking it", and those need different actions.
    /// Called only for the few that vanished, so the common run costs one query.
    fn pull_request_state(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError>;

    fn newest_comment(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError>;
}

/// Backed by the hosting service's command line tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct CliForge;

impl CliForge {
    fn run(repo: &Path, args: &[&str]) -> Result<String, ForgeError> {
        let output = Command::new("gh").args(args).current_dir(repo).output()?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(ForgeError::Command {
            command: format!("gh {}", args.join(" ")),
            dir: repo.display().to_string(),
            code: output.status.code().unwrap_or(-1),
            // Without the stderr an authentication failure reads only as "exit
            // status 4", which cost real diagnosis time.
            stderr: if stderr.is_empty() {
                "no stderr".to_owned()
            } else {
                stderr
            },
        })
    }
}

impl Forge for CliForge {
    fn pull_requests(&self, repo: &Path) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
        // Every state, not just open. A pull request the maintainer declined is
        // the most important thing to know about a branch, and querying only open
        // ones reported "no pull request" and then advised opening one.
        let payload = Self::run(repo, &PR_LIST_ARGS)?;
        Ok(index_by_branch(&parse_pull_requests(&payload)?))
    }

    fn pull_details(
        &self,
        repo: &Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullDetails>, ForgeError> {
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        // Two subprocesses, not one: the GraphQL endpoint has no repository
        // context of its own, so the owner and name come from the same resolution
        // `gh pr list` uses — whatever the remotes and the resolved-repository
        // markers say, rather than a second guess of our own.
        let named = Self::run(repo, &["repo", "view", "--json", "nameWithOwner"])?;
        let (owner, name) = parse_repo_target(&named)?;
        let payload = Self::run(
            repo,
            &[
                "api",
                "graphql",
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-f",
                &format!("query={}", pull_details_query(numbers)),
            ],
        )?;
        parse_pull_details(&payload)
    }

    fn pull_request_state(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError> {
        let payload = Self::run(
            repo,
            &["pr", "view", &number.to_string(), "--json", "state"],
        )?;
        Ok(parse_state(&payload)?)
    }

    fn newest_comment(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError> {
        let payload = Self::run(
            repo,
            &[
                "pr",
                "view",
                &number.to_string(),
                "--json",
                "comments,reviews",
            ],
        )?;
        parse_newest_comment(&payload)
    }
}

#[derive(Deserialize)]
struct StateOnly {
    state: String,
}

pub fn parse_state(payload: &str) -> Result<Option<String>, serde_json::Error> {
    let parsed: StateOnly = serde_json::from_str(payload)?;
    Ok(Some(parsed.state))
}
/// One aliased field per number, so the reply carries exactly the pull requests
/// asked about and nothing else.
///
/// Alias names are not load-bearing: every entry repeats its own `number` and the
/// parser keys on that, so a forge that normalises aliases cannot silently
/// reassign a rollup to the wrong pull request. `commits(last: 1)` is where the
/// rollup lives — a pull request has no rollup of its own, only its head commit
/// does — and the connections are bounded because an unbounded one is a rejected
/// query rather than a slow one.
// `last` keeps the newest items, so newest review and newest commit are
// inside every truncation and need no pagination guard.
pub fn pull_details_query(numbers: &[u64]) -> String {
    let fields: String = numbers
        .iter()
        .map(|number| format!("p{number}: pullRequest(number: {number}) {{ ...details }}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner: String!, $name: String!) {{ \
         repository(owner: $owner, name: $name) {{ {fields} }} }} \
         fragment details on PullRequest {{ number \
         reviews(last: 100) {{ nodes {{ submittedAt }} }} \
         commits(last: 100) {{ nodes {{ commit {{ committedDate }} }} }} \
         rollup: commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ \
         contexts(first: 100) {{ pageInfo {{ hasNextPage }} nodes {{ __typename \
         ... on CheckRun {{ name conclusion }} \
         ... on StatusContext {{ context state }} }} }} }} }} }} }} }}"
    )
}

#[derive(Deserialize)]
struct RepoTarget {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

/// The owner and name to query, from the forge's own answer about this checkout.
pub fn parse_repo_target(payload: &str) -> Result<(String, String), ForgeError> {
    let target: RepoTarget = serde_json::from_str(payload)?;
    target
        .name_with_owner
        .split_once('/')
        .map(|(owner, name)| (owner.to_owned(), name.to_owned()))
        .ok_or(ForgeError::Target {
            named: target.name_with_owner,
        })
}

pub fn parse_pull_requests(payload: &str) -> Result<Vec<PullRequest>, ForgeError> {
    Ok(serde_json::from_str(payload)?)
}

pub fn index_by_branch(prs: &[PullRequest]) -> BTreeMap<BranchName, PullRequest> {
    let mut indexed = BTreeMap::new();
    for pr in prs {
        let _ = indexed
            .entry(BranchName::new(pr.head_ref_name.clone()))
            .or_insert_with(|| pr.clone());
    }
    indexed
}

#[derive(Deserialize)]
struct Dated {
    #[serde(rename = "submittedAt")]
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct Committed {
    #[serde(rename = "committedDate")]
    committed_date: String,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct Nodes<T> {
    #[serde(default, rename = "pageInfo")]
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Committed,
}

#[derive(Deserialize)]
struct RollupNode {
    commit: RollupHolder,
}

#[derive(Deserialize)]
struct RollupHolder {
    #[serde(default, rename = "statusCheckRollup")]
    rollup: Option<Contexts>,
}

#[derive(Deserialize, Default)]
struct PageInfo {
    #[serde(default, rename = "hasNextPage")]
    has_next_page: bool,
}

#[derive(Deserialize)]
struct Contexts {
    #[serde(default)]
    contexts: Option<Nodes<CheckRun>>,
}

#[derive(Deserialize)]
struct DetailsPayload {
    number: u64,
    #[serde(default)]
    reviews: Option<Nodes<Dated>>,
    #[serde(default)]
    commits: Option<Nodes<CommitNode>>,
    #[serde(default)]
    rollup: Option<Nodes<RollupNode>>,
}

#[derive(Deserialize)]
struct DetailsData {
    #[serde(default)]
    repository: Option<BTreeMap<String, Option<DetailsPayload>>>,
}

#[derive(Deserialize)]
struct QueryFailure {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct DetailsEnvelope {
    #[serde(default)]
    data: Option<DetailsData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

/// Review age and check state per pull request, from one batch reply.
///
/// A review four days older than the branch head sent an agent to rewrite
/// already-fixed code; that comparison is why the review timeline is asked for at
/// all. `CliForge::run` normally catches GraphQL errors first when `gh` exits
/// nonzero; the parser guard is defence-in-depth for an exit-zero reply that
/// still carries errors. An empty answer reads as "nothing to compare" and "no
/// checks", which is how a red pull request reads as clean.
pub fn parse_pull_details(payload: &str) -> Result<BTreeMap<u64, PullDetails>, ForgeError> {
    let envelope: DetailsEnvelope = serde_json::from_str(payload)?;
    // Deliberately discard partial answers: one loud batch problem, never salvage `data.repository` alongside errors.
    if !envelope.errors.is_empty() {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let mut details = BTreeMap::new();
    // No errors AND no repository is not an empty answer. This is only ever
    // called for a non-empty query — `CliForge::pull_details` returns early
    // otherwise — so a reply carrying neither is a reply about nothing, and
    // reporting it as "nothing to compare, no checks ran" is exactly how a red
    // pull request reads as clean.
    let Some(repository) = envelope.data.and_then(|data| data.repository) else {
        return Err(ForgeError::Query {
            detail: "the reply carried neither errors nor a repository".to_owned(),
        });
    };
    for payload in repository.into_values().flatten() {
        let newest_review = payload
            .reviews
            .iter()
            .flat_map(|list| list.nodes.iter())
            .filter_map(|review| review.submitted_at.as_deref())
            .max();
        let newest_commit = payload
            .commits
            .iter()
            .flat_map(|list| list.nodes.iter())
            .map(|node| node.commit.committed_date.as_str())
            .max();
        let review_predates_head = match (newest_review, newest_commit) {
            (Some(review), Some(commit)) => Some(review < commit),
            _ => None,
        };
        let has_more_contexts = payload
            .rollup
            .iter()
            .flat_map(|list| list.nodes.iter())
            .filter_map(|node| node.commit.rollup.as_ref())
            .filter_map(|rollup| rollup.contexts.as_ref())
            .any(|contexts| contexts.page_info.has_next_page);
        if has_more_contexts {
            return Err(ForgeError::Query {
                detail: format!(
                    "pull request #{} has more than 100 check contexts; refusing a truncated rollup",
                    payload.number
                ),
            });
        }

        // Always `Some` for a pull request the reply carried: it was consulted,
        // and an absent rollup means nothing ran rather than nobody asked.
        let checks = Some(ChecksSummary {
            runs: payload
                .rollup
                .iter()
                .flat_map(|list| list.nodes.iter())
                .filter_map(|node| node.commit.rollup.as_ref())
                .filter_map(|rollup| rollup.contexts.as_ref())
                .flat_map(|contexts| contexts.nodes.iter())
                .cloned()
                .collect(),
        });
        let _ = details.insert(
            payload.number,
            PullDetails {
                review_predates_head,
                checks,
            },
        );
    }
    Ok(details)
}

#[derive(Deserialize)]
struct Timestamped {
    #[serde(default, alias = "submittedAt", alias = "createdAt")]
    at: String,
}

#[derive(Deserialize)]
struct CommentPayload {
    #[serde(default)]
    comments: Vec<Timestamped>,
    #[serde(default)]
    reviews: Vec<Timestamped>,
}

pub fn parse_newest_comment(payload: &str) -> Result<Option<String>, ForgeError> {
    let parsed: CommentPayload = serde_json::from_str(payload)?;
    Ok(parsed
        .comments
        .iter()
        .chain(parsed.reviews.iter())
        .map(|item| item.at.clone())
        .filter(|at| !at.is_empty())
        .max())
}

/// Facts supplied directly, for tests.
#[derive(Debug, Default, Clone)]
pub struct FakeForge {
    pub pull_requests: BTreeMap<BranchName, PullRequest>,
    pub stale_reviews: Vec<u64>,
    pub checks: BTreeMap<u64, ChecksSummary>,
    /// States for numbers no longer in the pull request list.
    pub vanished_states: BTreeMap<u64, String>,
    pub newest_comments: BTreeMap<u64, String>,
}

impl Forge for FakeForge {
    fn pull_requests(&self, _repo: &Path) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
        Ok(self.pull_requests.clone())
    }

    fn pull_details(
        &self,
        _repo: &Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullDetails>, ForgeError> {
        Ok(numbers
            .iter()
            .map(|number| {
                let known = self
                    .pull_requests
                    .values()
                    .any(|pull_request| pull_request.number == *number);
                (
                    *number,
                    PullDetails {
                        // A pull request the fake does not know has nothing to
                        // compare, exactly as the real forge answers for one whose
                        // timeline it cannot see.
                        review_predates_head: known.then(|| self.stale_reviews.contains(number)),
                        checks: self.checks.get(number).cloned(),
                    },
                )
            })
            .collect())
    }

    fn pull_request_state(&self, _repo: &Path, number: u64) -> Result<Option<String>, ForgeError> {
        Ok(self.vanished_states.get(&number).cloned())
    }

    fn newest_comment(&self, _repo: &Path, number: u64) -> Result<Option<String>, ForgeError> {
        Ok(self.newest_comments.get(&number).cloned())
    }
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

    #[test]
    fn a_pull_request_from_someone_elses_fork_is_not_ours() {
        // A real repository had an outside contributor whose head branch was called `main`.
        // Because we carry a local `main`, name matching claimed it as our work.
        use super::{Account, PullRequest, ours_only};
        use crate::ids::BranchName;
        use std::collections::BTreeMap;

        let origin = format!("https://{HOST}/our-org/some-repo.git");
        let make = |number: u64, owner: Option<&str>| PullRequest {
            number,
            head_ref_name: "main".to_owned(),
            head_repository_owner: owner.map(|login| Account {
                login: login.to_owned(),
            }),
            ..PullRequest::default()
        };
        let mut pull_requests = BTreeMap::new();
        let _ = pull_requests.insert(BranchName::new("main"), make(4554, Some("outsider")));
        let kept = ours_only(pull_requests, &[&origin]);
        assert!(kept.is_empty(), "another owner's branch is not our work");

        let mut mine = BTreeMap::new();
        let _ = mine.insert(BranchName::new("main"), make(1, Some("our-org")));
        assert_eq!(ours_only(mine, &[&origin]).len(), 1);

        // A deleted head repository cannot be ours, and must not be assumed to be.
        let mut gone = BTreeMap::new();
        let _ = gone.insert(BranchName::new("main"), make(2, None));
        assert!(ours_only(gone, &[&origin]).is_empty());
    }

    #[test]
    fn a_head_on_the_release_remotes_owner_is_ours_too() {
        // Six real forks had origin pointed at an org copy while PR heads lived on a
        // personal fork recorded under another role. Matching only origin's owner
        // reported those PRs as nobody's and their branches as unpushed for months.
        use super::{Account, PullRequest, ours_only};
        use crate::ids::BranchName;
        use std::collections::BTreeMap;
        let origin = format!("https://{HOST}/org-copy/some-repo.git");
        let release = format!("https://{HOST}/personal/some-repo.git");
        let mut prs = BTreeMap::new();
        let _ = prs.insert(
            BranchName::new("feat/a"),
            PullRequest {
                number: 7,
                head_repository_owner: Some(Account {
                    login: "personal".to_owned(),
                }),
                ..PullRequest::default()
            },
        );
        assert_eq!(ours_only(prs.clone(), &[&origin, &release]).len(), 1);
        assert!(
            ours_only(prs, &[&origin]).is_empty(),
            "origin alone must not match"
        );
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

    const LIST: &str = r#"[
      {"number":1128,"state":"OPEN","reviewDecision":"REVIEW_REQUIRED",
       "headRefName":"feat/alpha","headRefOid":"53a0e91f","updatedAt":"2026-07-30T02:22:16Z",
       "isDraft":false,"url":"https://example.invalid/1128"},
      {"number":1124,"state":"OPEN","reviewDecision":"",
       "headRefName":"feat/beta","headRefOid":"e433eca5","updatedAt":"2026-07-29T19:44:19Z",
       "isDraft":true,"url":"https://example.invalid/1124"}
    ]"#;

    #[test]
    fn pull_requests_parse_and_index_by_head_branch() {
        let parsed = parse_pull_requests(LIST).unwrap();
        let indexed = index_by_branch(&parsed);
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[&BranchName::new("feat/alpha")].number, 1128);
        assert!(indexed[&BranchName::new("feat/beta")].is_draft);
    }

    #[test]
    fn a_merge_commit_parses_when_present_and_defaults_when_absent() {
        // Given: one merged pull request naming its merge commit, one open without
        let payload = r#"[
          {"number":7,"state":"MERGED","headRefName":"feat/alpha",
           "headRefOid":"53a0e91f","updatedAt":"2026-08-01T00:00:00Z",
           "mergeCommit":{"oid":"feedfacefeedfacefeedfacefeedfacefeedface"}},
          {"number":8,"state":"OPEN","headRefName":"feat/beta",
           "headRefOid":"e433eca5","updatedAt":"2026-08-01T00:00:00Z",
           "mergeCommit":null}
        ]"#;

        // When: the list payload is parsed
        let parsed = parse_pull_requests(payload).expect("parse");
        let indexed = index_by_branch(&parsed);

        // Then: the merge commit survives, and its absence stays None
        let merged = &indexed[&BranchName::new("feat/alpha")];
        assert!(merged.is_merged(), "a MERGED state is merged");
        assert_eq!(
            merged.merge_commit.as_ref().map(|merge| merge.oid.as_str()),
            Some("feedfacefeedfacefeedfacefeedfacefeedface")
        );
        let open = &indexed[&BranchName::new("feat/beta")];
        assert!(!open.is_merged());
        assert_eq!(open.merge_commit, None);
    }

    #[test]
    fn only_merged_pull_requests_onto_the_trunk_mark_landing_points() {
        // Given: pull requests in every state, plus a merge onto another base
        let record = |number: u64, state: &str, base: &str, oid: Option<&str>| PullRequest {
            number,
            state: state.to_owned(),
            base_ref_name: base.to_owned(),
            merge_commit: oid.map(|oid| MergeCommit {
                oid: oid.to_owned(),
            }),
            ..PullRequest::default()
        };
        let mut pull_requests = BTreeMap::new();
        let _ = pull_requests.insert(
            BranchName::new("e"),
            record(5, "merged", "main", Some("e5")),
        );
        let _ = pull_requests.insert(
            BranchName::new("a"),
            record(9, "MERGED", "main", Some("a9")),
        );
        let _ = pull_requests.insert(BranchName::new("b"), record(2, "OPEN", "main", None));
        let _ = pull_requests.insert(BranchName::new("c"), record(3, "CLOSED", "main", None));
        let _ = pull_requests.insert(BranchName::new("d"), record(4, "MERGED", "dev", Some("d4")));
        let _ = pull_requests.insert(BranchName::new("f"), record(6, "MERGED", "main", None));

        // When: the landing points on the trunk are read
        let landed = merged_onto(&pull_requests, "main");

        // Then: only trunk-based merges remain, in number order. A merged pull
        // request without a recorded merge commit stays listed with no landing
        // point: the caller cannot place it, and must refuse rather than rebase
        // to a target that quietly leaves merged work out.
        let brief: Vec<(u64, &str, Option<&str>)> = landed
            .iter()
            .map(|pull| (pull.number, pull.branch.as_str(), pull.oid.as_deref()))
            .collect();
        assert_eq!(
            brief,
            vec![(5, "e", Some("e5")), (6, "f", None), (9, "a", Some("a9")),],
            "case-insensitive state, trunk base only, sorted by number"
        );
    }

    /// The batch reply's shape, with one pull request per aliased field.
    fn details_payload(entries: &str) -> String {
        format!("{{\"data\":{{\"repository\":{{{entries}}}}}}}")
    }

    #[test]
    fn a_failing_check_is_told_from_one_that_never_ran_and_from_one_not_asked_about() {
        // Three states, and conflating any two of them misreports a pull request:
        // red CI, green-or-nothing-ran, and never consulted.
        let payload = details_payload(
            "\"p11\":{\"number\":11,\"rollup\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":\
             {\"contexts\":{\"nodes\":[\
             {\"__typename\":\"CheckRun\",\"conclusion\":\"FAILURE\",\"name\":\"build\"},\
             {\"__typename\":\"CheckRun\",\"conclusion\":\"SUCCESS\",\"name\":\"lint\"}]}}}}]}},\
             \"p12\":{\"number\":12,\"rollup\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":null}}]}}",
        );

        let details = parse_pull_details(&payload).expect("parse");

        let failing = details[&11].checks.as_ref().expect("consulted");
        assert!(failing.failing(), "a FAILURE conclusion is failing");
        assert_eq!(failing.failed_names(), vec!["build".to_owned()]);
        assert!(failing.ran());
        let quiet = details[&12].checks.as_ref().expect("consulted");
        assert!(!quiet.failing(), "an absent rollup is not a failure");
        assert!(!quiet.ran(), "an absent rollup means nothing ran");
        assert!(
            !details.contains_key(&13),
            "a number the reply did not carry is not consulted, not empty"
        );
    }

    #[test]
    fn an_error_status_context_is_not_silently_treated_as_still_running() {
        // External CI posting commit statuses reports an aborted build this way, and
        // missing it made a red pull request read as clean green.
        let payload = details_payload(
            "\"p11\":{\"number\":11,\"rollup\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":\
             {\"contexts\":{\"nodes\":[{\"__typename\":\"StatusContext\",\"context\":\"legacy-ci\",\
             \"state\":\"ERROR\"}]}}}}]}}",
        );

        let details = parse_pull_details(&payload).expect("parse");

        let checks = details[&11].checks.as_ref().expect("consulted");
        assert!(checks.failing(), "an ERROR state is failing");
        assert_eq!(checks.failed_names(), vec!["legacy-ci".to_owned()]);
    }

    #[test]
    fn a_review_is_stale_current_or_incomparable_and_the_three_stay_distinct() {
        // A review four days older than the branch head sent an agent to rewrite
        // already-fixed code. "No review exists" is not "the review is current".
        let payload = details_payload(
            "\"p1\":{\"number\":1,\"reviews\":{\"nodes\":[{\"submittedAt\":\"2026-07-01T00:00:00Z\"}]},\
             \"commits\":{\"nodes\":[{\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}},\
             \"p2\":{\"number\":2,\"reviews\":{\"nodes\":[{\"submittedAt\":\"2026-07-03T00:00:00Z\"}]},\
             \"commits\":{\"nodes\":[{\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}},\
             \"p3\":{\"number\":3,\"reviews\":{\"nodes\":[]},\
             \"commits\":{\"nodes\":[{\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}}",
        );

        let details = parse_pull_details(&payload).expect("parse");

        assert_eq!(details[&1].review_predates_head, Some(true));
        assert_eq!(details[&2].review_predates_head, Some(false));
        assert_eq!(details[&3].review_predates_head, None);
    }

    #[test]
    fn the_newest_review_and_the_newest_commit_decide_it_rather_than_the_last_listed() {
        // The reply's node order is the forge's business, not ours.
        let payload = details_payload(
            "\"p1\":{\"number\":1,\"reviews\":{\"nodes\":[\
             {\"submittedAt\":\"2026-07-05T00:00:00Z\"},{\"submittedAt\":\"2026-07-01T00:00:00Z\"}]},\
             \"commits\":{\"nodes\":[\
             {\"commit\":{\"committedDate\":\"2026-07-04T00:00:00Z\"}},\
             {\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}}",
        );
        assert_eq!(
            parse_pull_details(&payload).expect("parse")[&1].review_predates_head,
            Some(false)
        );
    }

    #[test]
    fn a_query_the_forge_rejected_is_an_error_rather_than_an_empty_answer() {
        // A partial answer that read as "nothing to compare" would render a red
        // pull request as clean, which is the whole failure class this raises for.
        let payload =
            "{\"data\":null,\"errors\":[{\"message\":\"Could not resolve to a Repository\"}]}";
        let error = parse_pull_details(payload).expect_err("errors must not be swallowed");
        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");
        assert!(
            error.to_string().contains("Could not resolve"),
            "was: {error}"
        );
    }

    #[test]
    fn a_reply_with_neither_errors_nor_a_repository_answers_nothing_loudly() {
        // The silent-fallback shape: no errors, no data, so every requested fact
        // would come back absent and every red pull request would read as clean.
        let error = parse_pull_details("{\"data\":{}}")
            .expect_err("a reply about nothing is not an answer");
        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");

        // But a repository that answered `null` for a number it does not have IS
        // an answer: that number was not consulted, and the boundary between the
        // two cases is the whole point.
        let present = parse_pull_details("{\"data\":{\"repository\":{\"p9\":null}}}")
            .expect("a repository that resolved is an answer");
        assert!(present.is_empty());
    }

    #[test]
    fn the_batch_query_asks_about_every_number_and_nothing_else() {
        let query = pull_details_query(&[1157, 4545]);
        assert!(query.contains("pullRequest(number: 1157)"), "was: {query}");
        assert!(query.contains("pullRequest(number: 4545)"), "was: {query}");
        assert!(query.contains("statusCheckRollup"), "was: {query}");
        assert!(query.contains("submittedAt"), "was: {query}");
        assert!(query.contains("committedDate"), "was: {query}");
        assert!(query.contains("hasNextPage"), "was: {query}");
        // Every entry repeats its own number, so alias names are not load-bearing.
        assert!(query.contains("number"), "was: {query}");
    }

    #[test]
    fn a_rollup_with_more_than_one_page_of_contexts_is_unavailable() {
        let contexts: Vec<serde_json::Value> = (0..100)
            .map(|number| {
                serde_json::json!({
                    "__typename": "CheckRun",
                    "name": format!("green-{number}"),
                    "conclusion": "SUCCESS",
                })
            })
            .collect();
        let payload = serde_json::json!({
            "data": {
                "repository": {
                    "p7": {
                        "number": 7,
                        "rollup": {
                            "nodes": [{
                                "commit": {
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "pageInfo": { "hasNextPage": true },
                                            "nodes": contexts,
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string();

        let error = parse_pull_details(&payload)
            .expect_err("a paginated check rollup must not render as complete");

        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");
        assert!(error.to_string().contains("#7"), "was: {error}");
        assert!(error.to_string().contains("more than 100"), "was: {error}");
    }

    #[test]
    fn a_repository_the_forge_will_not_split_into_owner_and_name_is_an_error() {
        assert_eq!(
            parse_repo_target("{\"nameWithOwner\":\"our-org/some-repo\"}").expect("split"),
            ("our-org".to_owned(), "some-repo".to_owned())
        );
        let error = parse_repo_target("{\"nameWithOwner\":\"bare\"}")
            .expect_err("a name with no owner cannot be queried");
        assert!(matches!(&error, ForgeError::Target { .. }), "was: {error}");
    }

    #[test]
    fn the_newest_comment_is_the_latest_of_both_kinds() {
        use super::parse_newest_comment;

        let payload = r#"{"comments":[{"createdAt":"2026-07-20T00:00:00Z"}],
                          "reviews":[{"submittedAt":"2026-07-28T00:00:00Z"}]}"#;
        assert_eq!(
            parse_newest_comment(payload).unwrap().as_deref(),
            Some("2026-07-28T00:00:00Z")
        );

        let empty = r#"{"comments":[],"reviews":[]}"#;
        assert_eq!(parse_newest_comment(empty).unwrap(), None);
    }

    #[test]
    fn a_comment_newer_than_every_review_is_the_newest_activity() {
        // Given: a comment that arrived after the newest review
        let payload = r#"{"comments":[{"createdAt":"2026-07-29T00:00:00Z"}],
                          "reviews":[{"submittedAt":"2026-07-28T00:00:00Z"}]}"#;

        // When: comment activity is parsed
        let newest = parse_newest_comment(payload).unwrap();

        // Then: the comment, not the older review, sets the high-water mark
        assert_eq!(newest.as_deref(), Some("2026-07-29T00:00:00Z"));
    }

    #[test]
    fn a_state_payload_parses() {
        assert_eq!(
            parse_state(r#"{"state":"MERGED"}"#).unwrap().as_deref(),
            Some("MERGED")
        );
    }

    #[test]
    fn the_fake_answers_a_review_only_for_a_pull_request_it_knows() {
        let fake = FakeForge::default();
        let details = fake.pull_details(Path::new("/tmp"), &[7]).expect("details");
        assert_eq!(details[&7].review_predates_head, None);
        assert_eq!(details[&7].checks, None);
    }

    #[test]
    fn the_fake_reports_checks_only_when_they_were_supplied() {
        // Given: one pull request with a returned check rollup
        let checks = ChecksSummary {
            runs: vec![CheckRun {
                name: "build".to_owned(),
                conclusion: "FAILURE".to_owned(),
            }],
        };
        let fake = FakeForge {
            checks: BTreeMap::from([(7, checks.clone())]),
            ..FakeForge::default()
        };

        // When: both are asked about in one call
        let details = fake
            .pull_details(Path::new("/tmp"), &[7, 8])
            .expect("details");

        // Then: unknown means not consulted, not an empty rollup
        assert_eq!(details[&7].checks, Some(checks));
        assert_eq!(details[&8].checks, None);
    }

    #[test]
    fn the_fake_reports_a_stale_review_for_a_pull_request_it_knows_is_stale() {
        let mut pull_requests = BTreeMap::new();
        let _ = pull_requests.insert(
            BranchName::new("feat/alpha"),
            PullRequest {
                number: 7,
                ..PullRequest::default()
            },
        );
        let fake = FakeForge {
            pull_requests,
            stale_reviews: vec![7],
            ..FakeForge::default()
        };
        let details = fake.pull_details(Path::new("/tmp"), &[7]).expect("details");
        assert_eq!(details[&7].review_predates_head, Some(true));
    }
}
