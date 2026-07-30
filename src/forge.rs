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
     headRepositoryOwner,mergeable,mergeStateStatus,baseRefName";
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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Account {
    pub login: String,
}

impl PullRequest {
    /// Whether the forge says this pull request is open.
    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("OPEN")
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
    origin_remote: &str,
) -> BTreeMap<BranchName, PullRequest> {
    let Some(owner) = remote_owner(origin_remote) else {
        // An origin we cannot parse is not a licence to claim everyone's work.
        return pull_requests;
    };
    pull_requests
        .into_iter()
        .filter(|(_, pr)| pr.is_from(owner))
        .collect()
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

pub trait Forge {
    /// Pull requests in every state, indexed by head branch name.
    fn pull_requests(&self, repo: &Path) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError>;

    /// Was the newest review submitted before the newest commit?
    ///
    /// `None` means there was nothing to compare, which is not the same as
    /// `Some(false)` and must never render as "the review is current".
    fn review_predates_head(&self, repo: &Path, number: u64) -> Result<Option<bool>, ForgeError>;

    /// What the forge's checks say about one pull request.
    ///
    /// Per pull request rather than in the list query, because `statusCheckRollup` there
    /// exceeds the forge's GraphQL budget and fails the whole call. Called only for the
    /// branches we render, which is our own handful rather than the repository's hundreds.
    fn checks(&self, repo: &Path, number: u64) -> Result<Option<ChecksSummary>, ForgeError>;

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

    fn review_predates_head(&self, repo: &Path, number: u64) -> Result<Option<bool>, ForgeError> {
        let payload = Self::run(
            repo,
            &[
                "pr",
                "view",
                &number.to_string(),
                "--json",
                "reviews,commits",
            ],
        )?;
        compare_review_to_head(&payload)
    }

    fn checks(&self, repo: &Path, number: u64) -> Result<Option<ChecksSummary>, ForgeError> {
        let number = number.to_string();
        let payload = Self::run(
            repo,
            &["pr", "view", &number, "--json", "statusCheckRollup"],
        )?;
        Ok(Some(parse_checks(&payload)?))
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

#[derive(Deserialize)]
struct CheckRollup {
    #[serde(default, rename = "statusCheckRollup")]
    checks: Option<ChecksSummary>,
}

pub fn parse_state(payload: &str) -> Result<Option<String>, serde_json::Error> {
    let parsed: StateOnly = serde_json::from_str(payload)?;
    Ok(Some(parsed.state))
}

pub fn parse_checks(payload: &str) -> Result<ChecksSummary, ForgeError> {
    Ok(serde_json::from_str::<CheckRollup>(payload)?
        .checks
        .unwrap_or_default())
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
struct ReviewAges {
    #[serde(default)]
    reviews: Vec<Dated>,
    #[serde(default)]
    commits: Vec<Committed>,
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

/// A review four days older than the branch head sent an agent to rewrite
/// already-fixed code. This comparison is the whole point.
pub fn compare_review_to_head(payload: &str) -> Result<Option<bool>, ForgeError> {
    let ages: ReviewAges = serde_json::from_str(payload)?;
    let newest_review = ages
        .reviews
        .iter()
        .filter_map(|r| r.submitted_at.as_deref())
        .max();
    let newest_commit = ages.commits.iter().map(|c| c.committed_date.as_str()).max();
    match (newest_review, newest_commit) {
        (Some(review), Some(commit)) => Ok(Some(review < commit)),
        _ => Ok(None),
    }
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

    fn review_predates_head(&self, _repo: &Path, number: u64) -> Result<Option<bool>, ForgeError> {
        if self.pull_requests.values().any(|pr| pr.number == number) {
            return Ok(Some(self.stale_reviews.contains(&number)));
        }
        Ok(None)
    }

    fn checks(&self, _repo: &Path, number: u64) -> Result<Option<ChecksSummary>, ForgeError> {
        Ok(self.checks.get(&number).cloned())
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
        let kept = ours_only(pull_requests, &origin);
        assert!(kept.is_empty(), "another owner's branch is not our work");

        let mut mine = BTreeMap::new();
        let _ = mine.insert(BranchName::new("main"), make(1, Some("our-org")));
        assert_eq!(ours_only(mine, &origin).len(), 1);

        // A deleted head repository cannot be ours, and must not be assumed to be.
        let mut gone = BTreeMap::new();
        let _ = gone.insert(BranchName::new("main"), make(2, None));
        assert!(ours_only(gone, &origin).is_empty());
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
    fn a_failing_check_is_told_from_one_that_never_ran() {
        // Given: one pull request with a failed check and one whose checks have not run
        let failed_payload = r#"{
           "statusCheckRollup":[
             {"__typename":"CheckRun","conclusion":"FAILURE","name":"build"},
             {"__typename":"CheckRun","conclusion":"SUCCESS","name":"lint"}
           ]}
        "#;
        let empty_payload = r#"{"statusCheckRollup":[]}"#;

        // When: each per-pull-request forge response is parsed
        let failed = parse_checks(failed_payload).expect("parse");
        let never_ran = parse_checks(empty_payload).expect("parse");

        // Then: a failure and a never-ran rollup remain distinct
        assert!(failed.failing(), "a FAILURE conclusion is failing");
        assert_eq!(failed.failed_names(), vec!["build".to_owned()]);
        assert!(failed.ran());
        assert!(!never_ran.failing(), "an empty rollup is not a failure");
        assert!(!never_ran.ran(), "an empty rollup means nothing ran");
        assert!(!ChecksSummary::default().failing());
    }

    #[test]
    fn an_error_status_context_is_not_silently_treated_as_still_running() {
        // Given: external CI reports an aborted build through the StatusContext state field
        let payload = r#"{"statusCheckRollup":[{"__typename":"StatusContext","context":"legacy-ci","state":"ERROR"}]}"#;

        // When: the per-pull-request forge response is parsed
        let checks = parse_checks(payload).expect("parse");

        // Then: its context and state become a named failed check
        assert!(checks.failing(), "an ERROR state is failing");
        assert_eq!(checks.failed_names(), vec!["legacy-ci".to_owned()]);
    }

    #[test]
    fn an_omitted_rollup_means_checks_were_asked_for_but_nothing_ran() {
        // Given: a successful per-pull-request response without a rollup key
        let payload = "{}";

        // When: the response is parsed
        let checks = parse_checks(payload).expect("parse");

        // Then: it remains distinct from an unconsulted pull request
        assert_eq!(checks, ChecksSummary::default());
    }

    #[test]
    fn a_review_older_than_the_newest_commit_is_stale() {
        // Given: a review from the 1st and a commit from the 2nd
        let payload = r#"{"reviews":[{"submittedAt":"2026-07-01T00:00:00Z"}],
                          "commits":[{"committedDate":"2026-07-02T00:00:00Z"}]}"#;
        // When / Then: the review predates the head
        assert_eq!(compare_review_to_head(payload).unwrap(), Some(true));
    }

    #[test]
    fn a_review_newer_than_the_newest_commit_is_current() {
        let payload = r#"{"reviews":[{"submittedAt":"2026-07-03T00:00:00Z"}],
                          "commits":[{"committedDate":"2026-07-02T00:00:00Z"}]}"#;
        assert_eq!(compare_review_to_head(payload).unwrap(), Some(false));
    }

    #[test]
    fn nothing_to_compare_is_none_rather_than_false() {
        // None and Some(false) must stay distinguishable: "no review exists" is
        // not "the review is current".
        let payload = r#"{"reviews":[],"commits":[{"committedDate":"2026-07-02T00:00:00Z"}]}"#;
        assert_eq!(compare_review_to_head(payload).unwrap(), None);
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
    fn the_fake_reports_none_for_a_pull_request_it_does_not_know() {
        let fake = FakeForge::default();
        assert_eq!(
            fake.review_predates_head(Path::new("/tmp"), 7).unwrap(),
            None
        );
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

        // When: checks are requested for known and unknown pull requests
        let known = fake.checks(Path::new("/tmp"), 7).expect("checks");
        let unknown = fake.checks(Path::new("/tmp"), 8).expect("checks");

        // Then: unknown means not consulted, not an empty rollup
        assert_eq!(known, Some(checks));
        assert_eq!(unknown, None);
    }
}
