//! `knives pr`: one pull request, one line, from a live read.

use std::path::Path;

use crate::config::RepoEntry;
use crate::forge::{DiffTotals, Forge, TimelineEvent, TimelineEventKind};
use crate::ids::RepoName;

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    pub number: u64,
    pub state: String,
    /// The head branch name — the association closed bare numbers were missing.
    pub branch: String,
    pub base: String,
    pub head: String,
    pub review: String,
    pub updated: String,
    pub url: String,
    pub is_draft: bool,
    pub mergeable: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref_deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip_commit_empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeline: Option<Vec<TimelineEvent>>,
}


pub struct Request<'a> {
    pub repo: &'a RepoName,
    pub entry: &'a RepoEntry,
    pub number: u64,
    pub timeline: bool,
    pub forge: &'a dyn Forge,
    pub cache_root: Option<&'a Path>,
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("repo", self.repo)
            .field("entry", self.entry)
            .field("number", &self.number)
            .field("forge", &true)
            .field("timeline", &self.timeline)
            .field("cache_root", &self.cache_root)
            .finish()
    }
}

/// `Ok(None)` when the forge answered and the number does not exist there.
pub fn gather(request: &Request<'_>) -> anyhow::Result<Option<Report>> {
    let remotes = [
        request.entry.remote(crate::config::Role::Origin),
        request.entry.remote(crate::config::Role::Release),
    ];
    let opened = crate::snapshot::open(crate::snapshot::SnapshotConfig {
        forge: request.forge,
        path: &request.entry.path,
        remotes,
        cache_root: request.cache_root,
    })?;
    let number = request.number;
    let snapshot = opened.complete_with(&number, |_, number| vec![*number])?;
    let timeline = if request.timeline && snapshot.fact(number).is_some() {
        let target = request.forge.repo_identity(&request.entry.path)?;
        Some(
            request
                .forge
                .pull_timeline(&request.entry.path, &target, number)?,
        )
    } else {
        None
    };
    let report = snapshot.fact(number).map(|fact| {
        let pull = &fact.pull;
        Report {
            repo: request.repo.to_string(),
            number,
            state: pull.state.clone(),
            branch: pull.head_ref_name.clone(),
            base: pull.base_ref_name.clone(),
            head: pull.head_ref_oid.clone(),
            review: pull.review_decision.clone(),
            updated: pull.updated_at.clone(),
            url: pull.url.clone(),
            is_draft: pull.is_draft,
            mergeable: pull.mergeable.clone(),
            diff: fact.details.diff,
            head_ref_deleted: fact.details.head_ref_deleted,
            tip_commit_empty: fact.details.tip_commit_empty,
            timeline,
        }
    });
    // The live batch is useful to subsequent reads even for a one-number request.
    let _ = snapshot.persist(None);
    Ok(report)
}

/// `ai#4545  CLOSED  feat/egress-guard -> main  @ab12cd34ef56  review APPROVED  updated …  <url>`
/// plus a trailing ` [empty-diff]`-style bracket for each answered incident flag.
pub fn render(report: &Report) -> String {
    let mut line = format!(
        "{}#{}  {}{}  {} -> {}  @{}  review {}  updated {}",
        report.repo,
        report.number,
        report.state,
        if report.is_draft { " (draft)" } else { "" },
        report.branch,
        report.base,
        report.head.chars().take(12).collect::<String>(),
        if report.review.is_empty() {
            "-"
        } else {
            &report.review
        },
        report.updated,
    );
    if let Some(diff) = report.diff
        && diff.empty()
    {
        line.push_str("  [empty-diff]");
    }
    if report.head_ref_deleted == Some(true) {
        line.push_str("  [deleted-head-ref]");
    }
    if report.tip_commit_empty == Some(true) {
        line.push_str("  [empty-tip-commit]");
    }
    if !report.url.is_empty() {
        line.push_str("  ");
        line.push_str(&report.url);
    }
    if let Some(events) = &report.timeline {
        for event in events {
            line.push('\n');
            line.push_str(&render_timeline_event(event));
        }
    }
    line
}

fn render_timeline_event(event: &TimelineEvent) -> String {
    match &event.kind {
        TimelineEventKind::ForcePush { before, after } => format!(
            "  {}  force-push  {} (tree {}) -> {} (tree {}){}",
            event.at,
            short(&before.commit),
            short(&before.tree),
            short(&after.commit),
            short(&after.tree),
            if before.tree != "unknown" && before.tree == after.tree {
                "  [same tree]"
            } else {
                ""
            }
        ),
        TimelineEventKind::HeadDeleted => format!("  {}  head-deleted", event.at),
        TimelineEventKind::HeadRestored => format!("  {}  head-restored", event.at),
        TimelineEventKind::Closed => format!("  {}  closed", event.at),
        TimelineEventKind::Reopened => format!("  {}  reopened", event.at),
        TimelineEventKind::Merged { commit } => commit.as_deref().map_or_else(
            || format!("  {}  merged", event.at),
            |commit| format!("  {}  merged  @{}", event.at, short(commit)),
        ),
    }
}

fn short(oid: &str) -> String {
    oid.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;
    use crate::forge::{DiffTotals, PullRequest};
    use crate::forge::fake::FakeForge;
    use crate::ids::{BranchName, RepoName};

    fn entry() -> crate::config::RepoEntry {
        crate::config::RepoEntry {
            path: std::path::PathBuf::new(),
            upstream: String::new(),
            origin: String::new(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        }
    }

    #[test]
    fn a_closed_pull_reports_its_branch_on_one_line() {
        let forge = FakeForge {
            pull_requests: [(
                BranchName::new("feat/egress-guard"),
                PullRequest {
                    number: 4545,
                    state: "CLOSED".to_owned(),
                    head_ref_name: "feat/egress-guard".to_owned(),
                    ..PullRequest::default()
                },
            )]
            .into(),
            ..FakeForge::default()
        };
        let entry = entry();
        let request = Request {
            repo: &RepoName::new("demo"),
            entry: &entry,
            number: 4545,
            forge: &forge,
            cache_root: None,
            timeline: false,
        };
        let report = gather(&request)
            .expect("gather")
            .expect("the fake answers 4545");
        assert_eq!(report.branch, "feat/egress-guard");
        let line = render(&report);
        assert!(
            line.contains("demo#4545") && line.contains("CLOSED"),
            "was: {line}"
        );
    }

    #[test]
    fn an_unanswered_number_is_none() {
        let entry = entry();
        let request = Request {
            repo: &RepoName::new("demo"),
            entry: &entry,
            number: 9,
            forge: &FakeForge::default(),
            cache_root: None,
            timeline: false,
        };
        assert!(
            gather(&request).expect("gather").is_none(),
            "the fake was not configured with #9"
        );
    }

    #[test]
    fn render_keeps_the_pull_requests_present_state_context_together() {
        let report = Report {
            repo: "demo".to_owned(),
            number: 4545,
            state: "CLOSED".to_owned(),
            branch: "feat/egress-guard".to_owned(),
            base: "main".to_owned(),
            head: "ab12cd34ef56dead".to_owned(),
            review: "APPROVED".to_owned(),
            updated: "2026-08-30T00:00:00Z".to_owned(),
            url: "https://example.test/demo/pull/4545".to_owned(),
            is_draft: true,
            mergeable: "MERGEABLE".to_owned(),
            diff: Some(DiffTotals::default()),
            head_ref_deleted: Some(true),
            tip_commit_empty: Some(true),
            timeline: None,
        };

        assert_eq!(
            render(&report),
            "demo#4545  CLOSED (draft)  feat/egress-guard -> main  @ab12cd34ef56  \
             review APPROVED  updated 2026-08-30T00:00:00Z  [empty-diff]  \
             [deleted-head-ref]  [empty-tip-commit]  https://example.test/demo/pull/4545"
        );
    }

    #[test]
    fn render_shows_force_push_trees_and_marks_content_identical_rewrites() {
        let report = Report {
            repo: "demo".to_owned(),
            number: 7,
            state: "OPEN".to_owned(),
            branch: "feat/a".to_owned(),
            base: "main".to_owned(),
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            review: String::new(),
            updated: "2026-08-30T22:41:02Z".to_owned(),
            url: String::new(),
            is_draft: false,
            mergeable: "MERGEABLE".to_owned(),
            diff: None,
            head_ref_deleted: None,
            tip_commit_empty: None,
            timeline: Some(vec![crate::forge::TimelineEvent {
                at: "2026-08-30T22:41:02Z".to_owned(),
                kind: crate::forge::TimelineEventKind::ForcePush {
                    before: crate::forge::CommitOids {
                        commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                        tree: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                    },
                    after: crate::forge::CommitOids {
                        commit: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
                        tree: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                    },
                },
            }]),
        };

        assert_eq!(
            render(&report),
            concat!(
                "demo#7  OPEN  feat/a -> main  @aaaaaaaaaaaa  review -  updated \
                 2026-08-30T22:41:02Z\n",
                "  2026-08-30T22:41:02Z  force-push  aaaaaaaaaaaa (tree bbbbbbbbbbbb) -> \
                 cccccccccccc (tree bbbbbbbbbbbb)  [same tree]"
            )
        );
    }

    #[test]
    fn render_does_not_call_unavailable_trees_the_same_tree() {
        let report = Report {
            repo: "demo".to_owned(),
            number: 7,
            state: "OPEN".to_owned(),
            branch: "feat/a".to_owned(),
            base: "main".to_owned(),
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            review: String::new(),
            updated: "2026-08-30T22:41:02Z".to_owned(),
            url: String::new(),
            is_draft: false,
            mergeable: "MERGEABLE".to_owned(),
            diff: None,
            head_ref_deleted: None,
            tip_commit_empty: None,
            timeline: Some(vec![crate::forge::TimelineEvent {
                at: "2026-08-30T22:41:02Z".to_owned(),
                kind: crate::forge::TimelineEventKind::ForcePush {
                    before: crate::forge::CommitOids {
                        commit: "unknown".to_owned(),
                        tree: "unknown".to_owned(),
                    },
                    after: crate::forge::CommitOids {
                        commit: "unknown".to_owned(),
                        tree: "unknown".to_owned(),
                    },
                },
            }]),
        };

        assert!(
            !render(&report).contains("[same tree]"),
            "unavailable tree ids cannot prove a content-identical rewrite"
        );
    }
}
