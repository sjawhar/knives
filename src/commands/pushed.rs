//! `knives pushed`: reconcile local bookmarks against the remotes that own them.

use std::collections::BTreeMap;

use crate::cli::Exit;
use crate::config::{RepoEntry, Role};
use crate::detect::BookmarkTips;
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, ReleaseScheme, RepoName, is_release_name,
};
use crate::jj::{self, Repo};
use crate::store::Store;

const LIVE_PATTERNS: [&str; 2] = ["refs/heads/*", "refs/pull/*/head"];

/// One branch reconciliation result.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    pub rows: Vec<Row>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// The local and live state of one requested bookmark.
#[derive(Debug, serde::Serialize)]
pub struct Row {
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    pub verdicts: Vec<Verdict>,
}

/// A fact learned by comparing one ref against the remote that owns it.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "kebab-case", tag = "verdict")]
pub enum Verdict {
    InSync,
    NotOnRemote {
        remote: String,
    },
    Differs {
        remote: String,
        remote_commit: String,
    },
    RemoteOnly {
        remote: String,
        remote_commit: String,
    },
    GoneEverywhere,
    PullHeadDiffers {
        number: u64,
        remote_commit: String,
    },
}

/// The data `reconcile` classifies without opening a checkout or contacting a remote.
pub(crate) struct ReconcileInput<'a> {
    pub tips_local: &'a BTreeMap<BranchName, CommitId>,
    pub origin_refs: &'a BTreeMap<String, CommitId>,
    pub release_refs: &'a BTreeMap<String, CommitId>,
    pub scheme: &'a ReleaseScheme,
    pub tracked: &'a BTreeMap<BranchName, u64>,
    pub requested: &'a [BranchName],
}

/// Live refs read once from each remote that owns a pushable ref class.
pub(crate) struct LiveRefs {
    origin: BTreeMap<String, CommitId>,
    release: Option<BTreeMap<String, CommitId>>,
}

impl LiveRefs {
    pub(crate) const fn origin(&self) -> &BTreeMap<String, CommitId> {
        &self.origin
    }

    pub(crate) fn release(&self) -> &BTreeMap<String, CommitId> {
        self.release.as_ref().map_or(&self.origin, |refs| refs)
    }
}

/// Read heads and pull refs once from origin and, when separate, once from release.
pub(crate) fn live_refs(entry: &RepoEntry) -> Result<LiveRefs, crate::jj::JjError> {
    let origin = jj::remote_refs(entry.remote(Role::Origin), &LIVE_PATTERNS)?;
    let release = (entry.remote(Role::Release) != entry.remote(Role::Origin))
        .then(|| jj::remote_refs(entry.remote(Role::Release), &LIVE_PATTERNS))
        .transpose()?;
    Ok(LiveRefs { origin, release })
}

/// Reconcile requested branch names, assigning each ref class to its owning remote role.
pub(crate) fn reconcile(input: &ReconcileInput<'_>) -> Vec<Row> {
    input
        .requested
        .iter()
        .map(|branch| reconcile_branch(input, branch))
        .collect()
}

fn reconcile_branch(input: &ReconcileInput<'_>, branch: &BranchName) -> Row {
    let (remote, refs) = if is_release_name(branch, input.scheme) {
        (Role::Release, input.release_refs)
    } else {
        (Role::Origin, input.origin_refs)
    };
    let remote = remote.to_string();
    let local = input.tips_local.get(branch);
    let remote_head = refs.get(&format!("refs/heads/{branch}"));
    let mut verdicts = match (local, remote_head) {
        (Some(local), Some(remote_head)) if local == remote_head => vec![Verdict::InSync],
        (Some(_), Some(remote_head)) => vec![Verdict::Differs {
            remote,
            remote_commit: remote_head.to_string(),
        }],
        (Some(_), None) => vec![Verdict::NotOnRemote { remote }],
        (None, Some(remote_head)) => vec![Verdict::RemoteOnly {
            remote,
            remote_commit: remote_head.to_string(),
        }],
        (None, None) => vec![Verdict::GoneEverywhere],
    };
    if let (Some(local), Some(number)) = (local, input.tracked.get(branch))
        && let Some(pull_head) = input.origin_refs.get(&format!("refs/pull/{number}/head"))
        && pull_head != local
    {
        verdicts.push(Verdict::PullHeadDiffers {
            number: *number,
            remote_commit: pull_head.to_string(),
        });
    }
    Row {
        branch: branch.to_string(),
        local: local.map(ToString::to_string),
        verdicts,
    }
}

/// Read live remote refs once per owning push remote, then reconcile requested bookmarks.
pub fn gather(repo: &RepoName, entry: &RepoEntry, store: &Store, named: &[String]) -> Report {
    let opened = match Repo::open(&entry.path) {
        Ok(opened) => opened,
        Err(error) => {
            return problem_report(
                repo,
                format!("could not open {}: {error}", entry.path.display()),
            );
        }
    };
    let tips = match opened.bookmark_tips() {
        Ok(tips) => local_tips(tips),
        Err(error) => {
            return problem_report(repo, format!("could not read local bookmarks: {error}"));
        }
    };
    let live = match live_refs(entry) {
        Ok(live) => live,
        Err(error) => {
            return problem_report(repo, format!("could not read live push refs: {error}"));
        }
    };
    let scheme = entry.release_scheme();
    let requested = requested(&tips, named);
    let tracked = requested
        .iter()
        .filter_map(|branch| {
            store
                .tracked_pull(&BranchTarget::new(repo.clone(), branch.clone()))
                .map(|number| (branch.clone(), number))
        })
        .collect();
    Report {
        repo: repo.to_string(),
        rows: reconcile(&ReconcileInput {
            tips_local: &tips,
            origin_refs: live.origin(),
            release_refs: live.release(),
            scheme: &scheme,
            tracked: &tracked,
            requested: &requested,
        }),
        problems: Vec::new(),
    }
}

pub(crate) fn local_tips(tips: BookmarkTips) -> BTreeMap<BranchName, CommitId> {
    tips.into_iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch) => Some((branch, commit)),
            BookmarkRef::Remote { .. } => None,
        })
        .collect()
}

fn requested(tips: &BTreeMap<BranchName, CommitId>, named: &[String]) -> Vec<BranchName> {
    if named.is_empty() {
        return tips.keys().cloned().collect();
    }
    let mut branches: Vec<BranchName> = named.iter().cloned().map(BranchName::new).collect();
    branches.sort();
    branches.dedup();
    branches
}

fn problem_report(repo: &RepoName, problem: String) -> Report {
    Report {
        repo: repo.to_string(),
        rows: Vec::new(),
        problems: vec![problem],
    }
}

/// Findings outrank a clean read, while a failed live remote read is incomplete.
pub fn exit_for(report: &Report) -> Exit {
    if !report.problems.is_empty() {
        return Exit::Incomplete;
    }
    if report
        .rows
        .iter()
        .flat_map(|row| &row.verdicts)
        .any(is_finding)
    {
        Exit::Findings
    } else {
        Exit::Ok
    }
}

const fn is_finding(verdict: &Verdict) -> bool {
    matches!(
        verdict,
        Verdict::NotOnRemote { .. }
            | Verdict::Differs { .. }
            | Verdict::RemoteOnly { .. }
            | Verdict::PullHeadDiffers { .. }
    )
}

/// Render each requested branch and all of its observed reconciliation facts.
pub fn render(report: &Report) -> String {
    let mut lines = String::new();
    for problem in &report.problems {
        append_line(&mut lines, &format!("{}: PROBLEM: {problem}", report.repo));
    }
    for row in &report.rows {
        append_line(&mut lines, &format!("{}: {}", report.repo, render_row(row)));
    }
    lines
}

/// The compact human line for one reconciliation row, reusable by estate reports.
pub(crate) fn render_row(row: &Row) -> String {
    let local = row
        .local
        .as_deref()
        .map_or_else(|| "no local bookmark".to_owned(), short);
    let verdicts = row
        .verdicts
        .iter()
        .map(|verdict| render_verdict(row, verdict))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}  {}  {verdicts}", row.branch, local)
}

fn append_line(lines: &mut String, line: &str) {
    if !lines.is_empty() {
        lines.push('\n');
    }
    lines.push_str(line);
}

fn render_verdict(row: &Row, verdict: &Verdict) -> String {
    match verdict {
        Verdict::InSync => "in sync".to_owned(),
        Verdict::NotOnRemote { remote } => format!("not on {remote}"),
        Verdict::Differs {
            remote,
            remote_commit,
        } => format!(
            "differs on {remote} (local {}, remote {})",
            row.local.as_deref().map_or_else(String::new, short),
            short(remote_commit)
        ),
        Verdict::RemoteOnly {
            remote,
            remote_commit,
        } => format!(
            "{remote} still has {} at {} (no local bookmark)",
            row.branch,
            short(remote_commit)
        ),
        Verdict::GoneEverywhere => "gone everywhere".to_owned(),
        Verdict::PullHeadDiffers {
            number,
            remote_commit,
        } => format!(
            "pull #{number} head is {} (local {})",
            short(remote_commit),
            row.local.as_deref().map_or_else(String::new, short)
        ),
    }
}

fn short(value: &str) -> String {
    value.chars().take(12).collect()
}
