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

// The wide lists never ask for mergeable/mergeStateStatus: GitHub computes them
// lazily per pull request, which made the 300-row list cost 16s on one fork and
// deterministically 502 on another. Merge-state is live-batch-only (I2).
const PR_SUMMARY_FIELDS: &str = "number,state,reviewDecision,headRefName,headRefOid,updatedAt,\
     isDraft,url,headRepositoryOwner,baseRefName,mergeCommit";
const SUMMARY_LIST_ARGS: [&str; 8] = [
    "pr", "list", "--state", PR_STATE, "--limit", "300", "--json", PR_SUMMARY_FIELDS,
];

pub const fn summary_list_args() -> &'static [&'static str; 8] {
    &SUMMARY_LIST_ARGS
}

/// The fields asked for by the cheap pull-request list.
pub const fn summary_fields() -> &'static str {
    PR_SUMMARY_FIELDS
}


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

/// The cheap row: wide lists, the cache, and discovery. Carries every list
/// field EXCEPT `mergeable`/`mergeStateStatus` — their absence from this type
/// is the field split, enforced structurally: a consumer cannot read
/// merge-state from a list row because the field does not exist.
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
    pub base_ref_name: String,
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
    pub id: String, // GraphQL node id
}

impl RepoIdentity {
    /// (owner, repo), or ForgeError::Target when the name has no slash.
    pub fn split(&self) -> Result<(&str, &str), ForgeError> {
        self.name_with_owner
            .split_once('/')
            .ok_or_else(|| ForgeError::Target {
                named: self.name_with_owner.clone(),
            })
    }
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

/// The account names whose full pull-request history is worth fetching.
///
/// Derived from the same remotes `ours_only` filters by. An organization owner
/// cannot author a pull request, so a name that happens to be an org merely
/// returns an empty search.
pub fn search_authors(remotes: &[&str]) -> Vec<String> {
    let mut authors: Vec<String> = remotes
        .iter()
        .filter_map(|remote| remote_owner(remote))
        .map(str::to_owned)
        .collect();
    authors.dedup();
    authors
}

/// Keep only the pull requests that come from our own copy of the repository.
pub fn ours_only(pull_requests: Vec<PullRequest>, remotes: &[&str]) -> Vec<PullRequest> {
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
        .filter(|pr| owners.iter().any(|owner| pr.is_from(owner)))
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
pub fn merged_onto(pull_requests: &[PullRequest], trunk: &str) -> Vec<LandedPull> {
    let mut landed: Vec<LandedPull> = pull_requests
        .iter()
        .filter(|pr| pr.is_merged() && pr.base_ref_name == trunk)
        .map(|pr| LandedPull {
            number: pr.number,
            branch: BranchName::new(pr.head_ref_name.clone()),
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
    /// Pull requests in every state, as the forge lists them (freshest first).
    ///
    /// The raw list, not an index: one head branch accumulates several pull
    /// requests over its life, and which of them a caller cares about differs —
    /// `status` wants a primary per branch plus the shadowed history
    /// (`index_pulls`), `sync` resolves tracked numbers and must see every
    /// state without one shadowing another.
    ///
    /// `authors` names whose full history to fetch beyond the newest-300
    /// window. On a busy upstream that window is other people's traffic: a
    /// real repository's newest 300 pull requests started at #4720 while our
    /// own closed submissions sat at #4526–#4674, so every one of them —
    /// review history included — was invisible to every consumer. One extra
    /// author-scoped query per name retrieves ours in every state, whatever
    /// the upstream's volume.
    fn pull_requests(
        &self,
        repo: &Path,
        authors: &[String],
    ) -> Result<Vec<PullRequest>, ForgeError>;

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
    /// NOT_FOUND for are absent from the map; any other failure is an error.
    fn pull_facts(
        &self,
        repo: &Path,
        target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError>;
}

/// Backed by the hosting service's command line tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct CliForge;

impl CliForge {
    fn run(repo: &Path, args: &[&str]) -> Result<String, ForgeError> {
        let started = std::time::Instant::now();
        let output = Command::new("gh").args(args).current_dir(repo).output()?;
        if crate::timing::enabled() {
            eprintln!("{}", crate::timing::call_line(started.elapsed(), repo, args));
        }
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

fn joined_forge_call<T>(
    result: std::thread::Result<Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    result.map_err(|_| ForgeError::Query {
        detail: "a forge worker panicked".to_owned(),
    })?
}

impl Forge for CliForge {
    fn repo_identity(&self, repo: &Path) -> Result<RepoIdentity, ForgeError> {
        let payload = Self::run(repo, &["repo", "view", "--json", "nameWithOwner,id"])?;
        parse_identity(&payload)
    }

    fn list_pull_requests(
        &self,
        repo: &Path,
        authors: &[String],
    ) -> Result<Vec<PullSummary>, ForgeError> {
        std::thread::scope(|scope| {
            let base = scope.spawn(|| {
                let payload = Self::run(repo, &SUMMARY_LIST_ARGS)?;
                parse_summaries(&payload)
            });
            let author_lists = authors
                .iter()
                .map(|author| {
                    scope.spawn(move || {
                        let search = format!("author:{author}");
                        let args = [
                            "pr",
                            "list",
                            "--state",
                            PR_STATE,
                            "--limit",
                            "300",
                            "--search",
                            &search,
                            "--json",
                            PR_SUMMARY_FIELDS,
                        ];
                        let payload = Self::run(repo, &args)?;
                        parse_summaries(&payload)
                    })
                })
                .collect::<Vec<_>>();

            let mut pull_requests = joined_forge_call(base.join())?;
            for author_list in author_lists {
                pull_requests.extend(joined_forge_call(author_list.join())?);
            }
            dedupe_summaries(&mut pull_requests);
            Ok(pull_requests)
        })
    }

    fn sweep(&self, repo: &Path, target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
        let (owner, name) = target.split()?;
        let owner = format!("owner={owner}");
        let name = format!("name={name}");
        let query = format!("query={}", sweep_query());
        let payload = Self::run(
            repo,
            &[
                "api", "graphql", "-f", &owner, "-f", &name, "-f", &query,
            ],
        )?;
        parse_sweep(&payload)
    }

    fn pull_facts(
        &self,
        repo: &Path,
        target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        let (owner, name) = target.split()?;
        let owner = format!("owner={owner}");
        let name = format!("name={name}");

        std::thread::scope(|scope| {
            let chunks = numbers
                .chunks(40)
                .map(|chunk| {
                    let owner = &owner;
                    let name = &name;
                    scope.spawn(move || {
                        let query = format!("query={}", pull_facts_query(chunk));
                        let payload = Self::run(
                            repo,
                            &[
                                "api", "graphql", "-f", owner, "-f", name, "-f", &query,
                            ],
                        )?;
                        parse_pull_facts(&payload, chunk)
                    })
                })
                .collect::<Vec<_>>();

            let mut facts = BTreeMap::new();
            for chunk in chunks {
                facts.extend(joined_forge_call(chunk.join())?);
            }
            Ok(facts)
        })
    }
    fn pull_requests(
        &self,
        repo: &Path,
        authors: &[String],
    ) -> Result<Vec<PullRequest>, ForgeError> {
        // Every state, not just open. A pull request the maintainer declined is
        // the most important thing to know about a branch, and querying only open
        // ones reported "no pull request" and then advised opening one.
        let payload = Self::run(repo, &PR_LIST_ARGS)?;
        let mut pull_requests = parse_pull_requests(&payload)?;
        for author in authors {
            let search = format!("author:{author}");
            let args = [
                "pr", "list", "--state", PR_STATE, "--limit", "300", "--search", &search, "--json",
                PR_FIELDS,
            ];
            let payload = Self::run(repo, &args)?;
            pull_requests.extend(parse_pull_requests(&payload)?);
        }
        dedupe_by_number(&mut pull_requests);
        Ok(pull_requests)
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

/// One page, newest-updated first. No pagination: cursoring over a changing
/// UPDATED_AT ordering can skip a concurrently-updated pull request, so a page
/// that does not span the watermark abandons the delta (snapshot::discover).
pub fn sweep_query() -> &'static str {
    "query($owner: String!, $name: String!) { \
     repository(owner: $owner, name: $name) { \
     pullRequests(orderBy: {field: UPDATED_AT, direction: DESC}, first: 100) { \
     pageInfo { hasNextPage } \
     nodes { number updatedAt state } } } }"
}

/// The full I2 fact row per aliased number: every summary field plus
/// merge-state, review timeline, check rollup, and the newest comment (sync).
/// Alias names are not load-bearing; the parser keys on the repeated `number`.
pub fn pull_facts_query(numbers: &[u64]) -> String {
    let fields: String = numbers
        .iter()
        .map(|number| format!("p{number}: pullRequest(number: {number}) {{ ...facts }}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner: String!, $name: String!) {{ \
         repository(owner: $owner, name: $name) {{ {fields} }} }} \
         fragment facts on PullRequest {{ number state reviewDecision headRefName headRefOid \
         updatedAt isDraft url headRepositoryOwner {{ login }} baseRefName mergeable \
         mergeStateStatus mergeCommit {{ oid }} \
         reviews(last: 100) {{ nodes {{ submittedAt }} }} \
         commits(last: 100) {{ nodes {{ commit {{ committedDate }} }} }} \
         rollup: commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ \
         contexts(first: 100) {{ pageInfo {{ hasNextPage }} nodes {{ __typename \
         ... on CheckRun {{ name conclusion }} \
         ... on StatusContext {{ context state }} }} }} }} }} }} }} \
         comments(last: 1) {{ nodes {{ createdAt }} }} }}"
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

/// Drop later duplicates of a number, keeping the forge's freshest-first order.
///
/// The windowed list and the author-scoped searches overlap on recent pull
/// requests; a duplicate would double a branch's history.
pub fn dedupe_by_number(pull_requests: &mut Vec<PullRequest>) {
    let mut seen = std::collections::BTreeSet::new();
    pull_requests.retain(|pull_request| seen.insert(pull_request.number));
}

/// Drop later duplicate summary rows, keeping the forge's freshest-first order.
pub fn dedupe_summaries(pull_requests: &mut Vec<PullSummary>) {
    let mut seen = std::collections::BTreeSet::new();
    pull_requests.retain(|pull_request| seen.insert(pull_request.number));
}

/// One branch's pull requests, split into the primary and its shadowed history.
#[derive(Debug, Default)]
pub struct PullIndex {
    /// The pull request a reader should look at first for each head branch.
    pub by_branch: BTreeMap<BranchName, PullRequest>,
    /// The rest of each branch's pull requests, in the forge's freshest-first
    /// order. A head branch accumulates several over its life — an org-fork
    /// submission closed and re-homed onto a personal fork keeps its review
    /// history on the closed number — and collapsing to one per branch used to
    /// discard these silently. An audit walked straight past a maintainer's
    /// blocking question because the closed predecessor carrying it never
    /// rendered anywhere.
    pub prior: BTreeMap<BranchName, Vec<PullRequest>>,
}

/// Index pull requests by head branch, keeping every shadowed one visible.
///
/// Primary selection is deterministic: an open pull request beats any closed or
/// merged one, and ties keep the forge's own ordering. First-wins list order —
/// the previous rule — let whichever pull request the forge listed first shadow
/// the rest, so a freshly closed duplicate could hide a still-open submission
/// and vice versa.
pub fn index_pulls(prs: &[PullRequest]) -> PullIndex {
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
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct DetailsEnvelope {
    #[serde(default)]
    data: Option<DetailsData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

#[derive(Deserialize)]
struct IdentityPayload {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    id: String,
}

#[derive(Deserialize)]
struct SweepEntryPayload {
    number: u64,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    state: String,
}

#[derive(Deserialize)]
struct SweepPullRequests {
    #[serde(default, rename = "pageInfo")]
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<SweepEntryPayload>,
}

#[derive(Deserialize)]
struct SweepRepository {
    #[serde(rename = "pullRequests")]
    pull_requests: SweepPullRequests,
}

#[derive(Deserialize)]
struct SweepData {
    #[serde(default)]
    repository: Option<SweepRepository>,
}

#[derive(Deserialize)]
struct SweepEnvelope {
    #[serde(default)]
    data: Option<SweepData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

#[derive(Deserialize)]
struct FactsPayload {
    #[serde(flatten)]
    pull: PullRequest,
    #[serde(default)]
    reviews: Option<Nodes<Dated>>,
    #[serde(default)]
    commits: Option<Nodes<CommitNode>>,
    #[serde(default)]
    rollup: Option<Nodes<RollupNode>>,
    #[serde(default)]
    comments: Option<Nodes<Created>>,
}

#[derive(Deserialize)]
struct Created {
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Deserialize)]
struct FactsData {
    #[serde(default)]
    repository: Option<BTreeMap<String, Option<FactsPayload>>>,
}

#[derive(Deserialize)]
struct FactsEnvelope {
    #[serde(default)]
    data: Option<FactsData>,
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

pub fn parse_identity(payload: &str) -> Result<RepoIdentity, ForgeError> {
    let identity: IdentityPayload = serde_json::from_str(payload)?;
    Ok(RepoIdentity {
        name_with_owner: identity.name_with_owner,
        id: identity.id,
    })
}

pub fn parse_sweep(payload: &str) -> Result<SweepPage, ForgeError> {
    let envelope: SweepEnvelope = serde_json::from_str(payload)?;
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
    let Some(pulls) = envelope
        .data
        .and_then(|data| data.repository)
        .map(|repository| repository.pull_requests)
    else {
        return Err(ForgeError::Query {
            detail: "the reply carried neither errors nor a repository".to_owned(),
        });
    };
    Ok(SweepPage {
        entries: pulls
            .nodes
            .into_iter()
            .map(|entry| SweepEntry {
                number: entry.number,
                updated_at: entry.updated_at,
                state: entry.state,
            })
            .collect(),
        has_next_page: pulls.page_info.has_next_page,
    })
}

pub fn parse_summaries(payload: &str) -> Result<Vec<PullSummary>, ForgeError> {
    Ok(serde_json::from_str(payload)?)
}

fn is_tolerable_not_found(failure: &QueryFailure, asked: &[u64]) -> bool {
    if failure.kind != "NOT_FOUND" {
        return false;
    }
    let [repository, alias] = failure.path.as_slice() else {
        return false;
    };
    let (Some(repository), Some(alias)) = (repository.as_str(), alias.as_str()) else {
        return false;
    };
    repository == "repository" && asked.iter().any(|number| alias == format!("p{number}"))
}

pub fn parse_pull_facts(
    payload: &str,
    asked: &[u64],
) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
    let envelope: FactsEnvelope = serde_json::from_str(payload)?;
    if envelope
        .errors
        .iter()
        .any(|failure| !is_tolerable_not_found(failure, asked))
    {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .filter(|failure| !is_tolerable_not_found(failure, asked))
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let Some(repository) = envelope.data.and_then(|data| data.repository) else {
        return Err(ForgeError::Query {
            detail: "the reply carried neither errors nor a repository".to_owned(),
        });
    };
    let mut facts = BTreeMap::new();
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
                    payload.pull.number
                ),
            });
        }

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
        let newest_comment = payload
            .reviews
            .iter()
            .flat_map(|list| list.nodes.iter())
            .filter_map(|review| review.submitted_at.as_deref())
            .chain(
                payload
                    .comments
                    .iter()
                    .flat_map(|list| list.nodes.iter())
                    .map(|comment| comment.created_at.as_str()),
            )
            .max()
            .map(str::to_owned);
        let number = payload.pull.number;
        let _ = facts.insert(
            number,
            PullFacts {
                pull: payload.pull,
                details: PullDetails {
                    review_predates_head,
                    checks,
                },
                newest_comment,
            },
        );
    }
    Ok(facts)
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
    /// States for numbers outside the listed universe (deleted-from-window
    /// history a batch can still answer about).
    pub vanished_states: BTreeMap<u64, String>,
    pub newest_comments: BTreeMap<u64, String>,
    pub fail_identity: bool,
    pub fail_list: bool,
    pub fail_sweep: bool,
    pub fail_facts: bool,
    /// Sweep reports a continuation past page 1 (overflow → cold reseed).
    pub sweep_overflows: bool,
}

fn fake_failure(operation: &str) -> ForgeError {
    ForgeError::Command {
        command: "fake".to_owned(),
        dir: "/fake".to_owned(),
        code: 1,
        stderr: format!("fake {operation} failed"),
    }
}

fn vanished_pull(number: u64, state: String) -> PullRequest {
    PullRequest {
        number,
        state,
        review_decision: String::new(),
        head_ref_name: String::new(),
        head_ref_oid: String::new(),
        updated_at: String::new(),
        is_draft: false,
        url: String::new(),
        head_repository_owner: None,
        mergeable: String::new(),
        merge_state_status: String::new(),
        base_ref_name: String::new(),
        merge_commit: None,
    }
}

impl Forge for FakeForge {
    fn repo_identity(&self, _repo: &Path) -> Result<RepoIdentity, ForgeError> {
        if self.fail_identity {
            return Err(fake_failure("identity"));
        }
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
        if self.fail_list {
            return Err(fake_failure("list"));
        }
        Ok(self.pull_requests.values().map(PullSummary::of).collect())
    }

    fn sweep(&self, _repo: &Path, _target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
        if self.fail_sweep {
            return Err(fake_failure("sweep"));
        }
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
            has_next_page: self.sweep_overflows,
        })
    }

    fn pull_facts(
        &self,
        _repo: &Path,
        _target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
        if self.fail_facts {
            return Err(fake_failure("facts"));
        }
        Ok(numbers
            .iter()
            .filter_map(|number| {
                let facts = if let Some(pull) = self
                    .pull_requests
                    .values()
                    .find(|pull| pull.number == *number)
                {
                    Some(PullFacts {
                        pull: pull.clone(),
                        details: PullDetails {
                            review_predates_head: Some(self.stale_reviews.contains(number)),
                            checks: self.checks.get(number).cloned(),
                        },
                        newest_comment: self.newest_comments.get(number).cloned(),
                    })
                } else {
                    self.vanished_states.get(number).map(|state| PullFacts {
                        pull: vanished_pull(*number, state.clone()),
                        details: PullDetails::default(),
                        newest_comment: self.newest_comments.get(number).cloned(),
                    })
                };
                facts.map(|facts| (*number, facts))
            })
            .collect())
    }
    fn pull_requests(
        &self,
        _repo: &Path,
        _authors: &[String],
    ) -> Result<Vec<PullRequest>, ForgeError> {
        Ok(self.pull_requests.values().cloned().collect())
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

        let origin = format!("https://{HOST}/our-org/some-repo.git");
        let make = |number: u64, owner: Option<&str>| PullRequest {
            number,
            head_ref_name: "main".to_owned(),
            head_repository_owner: owner.map(|login| Account {
                login: login.to_owned(),
            }),
            ..PullRequest::default()
        };
        let kept = ours_only(vec![make(4554, Some("outsider"))], &[&origin]);
        assert!(kept.is_empty(), "another owner's branch is not our work");

        assert_eq!(
            ours_only(vec![make(1, Some("our-org"))], &[&origin]).len(),
            1
        );

        // A deleted head repository cannot be ours, and must not be assumed to be.
        assert!(ours_only(vec![make(2, None)], &[&origin]).is_empty());
    }

    #[test]
    fn a_head_on_the_release_remotes_owner_is_ours_too() {
        // Six real forks had origin pointed at an org copy while PR heads lived on a
        // personal fork recorded under another role. Matching only origin's owner
        // reported those PRs as nobody's and their branches as unpushed for months.
        use super::{Account, PullRequest, ours_only};
        let origin = format!("https://{HOST}/org-copy/some-repo.git");
        let release = format!("https://{HOST}/personal/some-repo.git");
        let prs = vec![PullRequest {
            number: 7,
            head_ref_name: "feat/a".to_owned(),
            head_repository_owner: Some(Account {
                login: "personal".to_owned(),
            }),
            ..PullRequest::default()
        }];
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
        let indexed = index_pulls(&parsed);
        assert_eq!(indexed.by_branch.len(), 2);
        assert_eq!(
            indexed.by_branch[&BranchName::new("feat/alpha")].number,
            1128
        );
        assert!(indexed.by_branch[&BranchName::new("feat/beta")].is_draft);
        assert!(indexed.prior.is_empty());
    }

    #[test]
    fn an_open_pull_request_beats_a_closed_one_whatever_the_list_order() {
        // A head branch accumulates pull requests over its life: an org-fork
        // submission closed and re-homed onto a personal fork keeps its review
        // history on the closed number. First-wins list order let whichever the
        // forge listed first shadow the rest silently.
        let record = |number: u64, state: &str| PullRequest {
            number,
            state: state.to_owned(),
            head_ref_name: "feat/alpha".to_owned(),
            ..PullRequest::default()
        };

        // Closed listed first (a freshly closed duplicate above an older open one).
        let closed_first = [record(9, "CLOSED"), record(7, "OPEN")];
        let indexed = index_pulls(&closed_first);
        assert_eq!(indexed.by_branch[&BranchName::new("feat/alpha")].number, 7);
        let prior = &indexed.prior[&BranchName::new("feat/alpha")];
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].number, 9);

        // Open listed first (the usual freshest-first order): same answer.
        let open_first = [record(7, "OPEN"), record(9, "CLOSED")];
        let indexed = index_pulls(&open_first);
        assert_eq!(indexed.by_branch[&BranchName::new("feat/alpha")].number, 7);
        assert_eq!(indexed.prior[&BranchName::new("feat/alpha")].len(), 1);

        // A branch whose only pull request is closed keeps it as primary: a
        // declined pull request is the most important thing to know about it.
        let indexed = index_pulls(&[record(9, "CLOSED")]);
        assert_eq!(indexed.by_branch[&BranchName::new("feat/alpha")].number, 9);
        assert!(indexed.prior.is_empty());
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
        let indexed = index_pulls(&parsed).by_branch;

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
        let with_branch = |branch: &str, pull_request: PullRequest| PullRequest {
            head_ref_name: branch.to_owned(),
            ..pull_request
        };
        let pull_requests = vec![
            with_branch("e", record(5, "merged", "main", Some("e5"))),
            with_branch("a", record(9, "MERGED", "main", Some("a9"))),
            with_branch("b", record(2, "OPEN", "main", None)),
            with_branch("c", record(3, "CLOSED", "main", None)),
            with_branch("d", record(4, "MERGED", "dev", Some("d4"))),
            with_branch("f", record(6, "MERGED", "main", None)),
        ];

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

    #[test]
    fn the_fake_sweep_is_newest_first_and_reports_overflow() {
        let pull_requests = BTreeMap::from([
            (
                BranchName::new("feat/older"),
                PullRequest {
                    number: 7,
                    updated_at: "2026-08-01T00:00:00Z".to_owned(),
                    ..PullRequest::default()
                },
            ),
            (
                BranchName::new("feat/newer"),
                PullRequest {
                    number: 9,
                    updated_at: "2026-08-02T00:00:00Z".to_owned(),
                    ..PullRequest::default()
                },
            ),
        ]);
        let fake = FakeForge {
            pull_requests,
            sweep_overflows: true,
            ..FakeForge::default()
        };
        let target = RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        };

        let sweep = fake.sweep(Path::new("/tmp"), &target).expect("sweep");

        assert!(sweep.has_next_page);
        assert_eq!(
            sweep
                .entries
                .iter()
                .map(|entry| entry.number)
                .collect::<Vec<_>>(),
            vec![9, 7]
        );
    }

    #[test]
    fn fake_facts_answer_the_universe_the_vanished_and_nothing_else() {
        let pull = PullRequest {
            number: 7,
            state: "OPEN".to_owned(),
            head_ref_name: "feat/known".to_owned(),
            ..PullRequest::default()
        };
        let fake = FakeForge {
            pull_requests: BTreeMap::from([(BranchName::new("feat/known"), pull)]),
            vanished_states: BTreeMap::from([(8, "CLOSED".to_owned())]),
            newest_comments: BTreeMap::from([(7, "2026-08-03T00:00:00Z".to_owned())]),
            ..FakeForge::default()
        };
        let target = RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        };

        let facts = fake
            .pull_facts(Path::new("/tmp"), &target, &[7, 8, 9])
            .expect("facts");

        assert_eq!(facts[&7].pull.head_ref_name, "feat/known");
        assert_eq!(
            facts[&7].newest_comment.as_deref(),
            Some("2026-08-03T00:00:00Z")
        );
        assert_eq!(facts[&8].pull.state, "CLOSED");
        assert!(!facts.contains_key(&9));
    }
    #[test]
    fn a_summary_row_has_no_merge_state_to_read() {
        // The field split is structural: this test documents it by parsing a payload
        // that CARRIES mergeable and asserting the summary type ignores it.
        let parsed = parse_summaries(
            r#"[{"number":7,"state":"OPEN","headRefName":"feat/a",
        "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z","mergeable":"CONFLICTING"}]"#,
        )
        .expect("summary row parses");
        assert_eq!(parsed[0].number, 7); // PullSummary has no mergeable field — enforced by the compiler.
    }

    #[test]
    fn a_sweep_page_reports_order_and_continuation() {
        let page = parse_sweep(
            r#"{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":true},
        "nodes":[{"number":9,"updatedAt":"2026-08-02T00:00:00Z","state":"OPEN"},
                 {"number":7,"updatedAt":"2026-08-01T00:00:00Z","state":"MERGED"}]}}}}"#,
        )
        .expect("sweep page parses");
        assert!(page.has_next_page);
        assert_eq!(page.entries[0].number, 9, "newest-updated first");
        assert_eq!(page.entries[1].state, "MERGED");
    }

    /// The live batch reply's shape, with one fact payload per aliased field.
    fn facts_payload(entries: &str) -> String {
        format!("{{\"data\":{{\"repository\":{{{entries}}}}}}}")
    }

    #[test]
    fn facts_carry_the_full_row_the_details_and_the_newest_comment() {
        // One alias carrying every summary field plus mergeable CONFLICTING,
        // headRepositoryOwner{login}, a review newer than the newest comment.
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","reviewDecision":"APPROVED","headRefName":"feat/a",
        "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z","isDraft":false,"url":"u",
        "headRepositoryOwner":{"login":"our-org"},"baseRefName":"main","mergeable":"CONFLICTING",
        "mergeStateStatus":"DIRTY","mergeCommit":null,
        "reviews":{"nodes":[{"submittedAt":"2026-08-02T00:00:00Z"}]},
        "commits":{"nodes":[{"commit":{"committedDate":"2026-08-01T12:00:00Z"}}]},
        "rollup":{"nodes":[]},"comments":{"nodes":[{"createdAt":"2026-07-30T00:00:00Z"}]}}"#,
        );
        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");
        let fact = &facts[&7];
        assert_eq!(fact.pull.mergeable, "CONFLICTING");
        assert_eq!(
            fact.pull.head_repository_owner.as_ref().map(|owner| owner.login.as_str()),
            Some("our-org")
        );
        assert_eq!(
            fact.details.review_predates_head,
            Some(false),
            "review is newer than the head"
        );
        assert_eq!(
            fact.newest_comment.as_deref(),
            Some("2026-08-02T00:00:00Z"),
            "the review outranks the older comment"
        );
    }

    #[test]
    fn a_not_found_alias_is_absent_and_does_not_fail_the_batch() {
        let payload = format!(
            r#"{{"data":{{"repository":{{"p7":{{"number":7,"state":"OPEN","headRefName":"feat/a",
            "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z"}},"p300":null}}}},
            "errors":[{{"type":"NOT_FOUND","path":["repository","p300"],"message":"not found"}}]}}"#
        );
        let facts = parse_pull_facts(&payload, &[7, 300]).expect("a missing asked alias is tolerated");
        assert!(facts.contains_key(&7));
        assert!(!facts.contains_key(&300));
    }

    #[test]
    fn any_other_error_fails_the_whole_batch() {
        let payload = r#"{"data":{"repository":{"p7":null}},"errors":[
            {"type":"RATE_LIMITED","path":["repository","p7"],"message":"rate limited"}]}"#;
        let error = parse_pull_facts(payload, &[7]).expect_err("only asked NOT_FOUND aliases are tolerated");
        assert!(matches!(error, ForgeError::Query { .. }), "was: {error}");
        assert!(error.to_string().contains("rate limited"), "was: {error}");
    }

    #[test]
    fn the_newest_comment_is_the_max_of_comments_and_reviews() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z",
            "reviews":{"nodes":[{"submittedAt":"2026-08-02T00:00:00Z"}]},
            "comments":{"nodes":[{"createdAt":"2026-08-03T00:00:00Z"}]}}"#,
        );
        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");
        assert_eq!(
            facts[&7].newest_comment.as_deref(),
            Some("2026-08-03T00:00:00Z")
        );
    }

    #[test]
    fn a_repo_identity_exposes_and_splits_the_forges_own_name() {
        let identity =
            parse_identity(r#"{"nameWithOwner":"our-org/some-repo","id":"R_kgDOxyz"}"#)
                .expect("identity parses");
        assert_eq!(identity.split().expect("owner/repo"), ("our-org", "some-repo"));

        let error = RepoIdentity {
            name_with_owner: "bare".to_owned(),
            id: "R_kgDOxyz".to_owned(),
        }
        .split()
        .expect_err("an unsplittable identity cannot be queried");
        assert!(matches!(error, ForgeError::Target { .. }), "was: {error}");
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
            mergeable: "CONFLICTING".to_owned(),
            merge_state_status: "DIRTY".to_owned(),
            base_ref_name: "main".to_owned(),
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

    #[test]
    fn cache_list_contract_omits_live_merge_state_fields() {
        let args = summary_list_args();
        assert_eq!(
            args,
            &["pr", "list", "--state", "all", "--limit", "300", "--json",
                "number,state,reviewDecision,headRefName,headRefOid,updatedAt,isDraft,url,\
     headRepositoryOwner,baseRefName,mergeCommit"]
        );
        assert!(!summary_fields().contains("mergeable"));
        assert!(!summary_fields().contains("mergeStateStatus"));
    }

    #[test]
    fn live_queries_request_the_required_fact_and_sweep_shapes() {
        let sweep = sweep_query();
        assert!(sweep.contains("orderBy: {field: UPDATED_AT, direction: DESC}"), "was: {sweep}");
        assert!(sweep.contains("first: 100"), "was: {sweep}");
        let facts = pull_facts_query(&[7, 300]);
        assert!(facts.contains("p7: pullRequest(number: 7)"), "was: {facts}");
        assert!(facts.contains("p300: pullRequest(number: 300)"), "was: {facts}");
        assert!(facts.contains("mergeable"), "was: {facts}");
        assert!(facts.contains("mergeStateStatus"), "was: {facts}");
        assert!(facts.contains("comments(last: 1)"), "was: {facts}");
    }
}
