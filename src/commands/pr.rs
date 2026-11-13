//! `knives pr`: one pull request, one line, from a live read.

use std::path::Path;

use crate::bind::Fork;
use crate::cli::Exit;
use crate::detect::pull_state::{PullState, pull_state_findings};
use crate::forge::{DiffTotals, Forge, PullDetails, TimelineEvent, TimelineEventKind};
use crate::ids::short_id;
use crate::ledger::inline_human_text;

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    pub number: u64,
    pub state: String,
    /// The head branch name — the association closed bare numbers were missing.
    pub branch: String,
    pub base: Option<String>,
    pub head: String,
    pub review: String,
    pub updated: String,
    pub url: String,
    pub is_draft: bool,
    pub mergeable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref_deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip_commit_empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeline: Option<Vec<TimelineEvent>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub struct Request<'a> {
    pub fork: &'a Fork<'a>,
    pub number: u64,
    pub timeline: bool,
    pub forge: &'a dyn Forge,
    pub cache_root: Option<&'a Path>,
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("fork", self.fork)
            .field("number", &self.number)
            .field("forge", &true)
            .field("timeline", &self.timeline)
            .field("cache_root", &self.cache_root)
            .finish()
    }
}

/// `Ok(None)` when the forge answered and the number does not exist there.
pub fn gather(request: &Request<'_>) -> anyhow::Result<Option<Report>> {
    let entry = request.fork.entry;
    let path = &request.fork.checkout.path;
    let remotes = [
        entry.remote(crate::config::Role::Origin),
        entry.remote(crate::config::Role::Release),
    ];
    let opened = crate::snapshot::open(crate::snapshot::SnapshotConfig {
        forge: request.forge,
        path,
        upstream: entry.remote(crate::config::Role::Upstream),
        remotes,
        cache_root: request.cache_root,
    })?;
    let number = request.number;
    let snapshot = opened.complete_with(&number, |_, number| vec![*number])?;
    let timeline = if request.timeline && snapshot.fact(number).is_some() {
        Some(
            request
                .forge
                .pull_timeline(path, opened.identity(), number)?,
        )
    } else {
        None
    };
    // The live batch is useful to subsequent reads even for a one-number request.
    let persist_note = snapshot
        .persist(None)
        .err()
        .map(|error| format!("could not persist forge snapshot: {error}"));
    let report = snapshot.fact(number).map(|fact| {
        let pull = &fact.pull;
        Report {
            repo: request.fork.name.to_string(),
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
            notes: persist_note.into_iter().collect(),
        }
    });
    Ok(report)
}

/// The `pr` command has complete data when the live snapshot succeeds. Its
/// optional cache write must remain visible, but cannot change the answer.
pub fn exit_for(report: &Report) -> Exit {
    let details = PullDetails {
        diff: report.diff,
        head_ref_deleted: report.head_ref_deleted,
        tip_commit_empty: report.tip_commit_empty,
        ..PullDetails::default()
    };
    if pull_state_findings(&[PullState {
        number: report.number,
        open: report.state.eq_ignore_ascii_case("OPEN"),
        details: &details,
    }])
    .is_empty()
    {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

/// `ai#4545  CLOSED  feat/egress-guard -> main  @ab12cd34ef56  review APPROVED  updated …  <url>`
/// plus a trailing ` [empty-diff]`-style bracket for each answered incident flag.
///
/// Every field but the registry key, the number and this module's own words
/// is the forge's answer — its state, the head and base branch names it
/// reports, its review decision, its url, its own timestamp text, the ids of
/// a force-push — so each reaches the terminal escaped
/// ([`inline_human_text`]), as `status`'s pull cell escapes the same answers.
/// A note quotes the cache write that could not be made.
pub fn render(report: &Report) -> String {
    let review = if report.review.is_empty() {
        "-".to_owned()
    } else {
        inline_human_text(&report.review)
    };
    let mut line = format!(
        "{}#{}  {}{}  {} -> {}  @{}  review {review}  updated {}",
        report.repo,
        report.number,
        inline_human_text(&report.state),
        if report.is_draft { " (draft)" } else { "" },
        inline_human_text(&report.branch),
        report
            .base
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), inline_human_text),
        short_forge_id(&report.head),
        inline_human_text(&report.updated),
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
        line.push_str(&inline_human_text(&report.url));
    }
    if let Some(events) = &report.timeline {
        for event in events {
            line.push('\n');
            line.push_str(&render_timeline_event(event));
        }
    }
    for note in &report.notes {
        line.push('\n');
        line.push_str("  note: ");
        line.push_str(&inline_human_text(note));
    }
    line
}

/// The twelve characters a forge-answered id shows, escaped. The escape maps
/// each character to one character, so the cut lands where it landed before
/// it, and the twelve shown are the whole of what reaches the terminal.
fn short_forge_id(id: &str) -> String {
    inline_human_text(short_id(id))
}

fn render_timeline_event(event: &TimelineEvent) -> String {
    let at = inline_human_text(&event.at);
    match &event.kind {
        TimelineEventKind::ForcePush { before, after } => format!(
            "  {at}  force-push  {} (tree {}) -> {} (tree {}){}",
            short_forge_id(&before.commit),
            short_forge_id(&before.tree),
            short_forge_id(&after.commit),
            short_forge_id(&after.tree),
            if before.tree != "unknown" && before.tree == after.tree {
                "  [same tree]"
            } else {
                ""
            }
        ),
        TimelineEventKind::HeadDeleted => format!("  {at}  head-deleted"),
        TimelineEventKind::HeadRestored => format!("  {at}  head-restored"),
        TimelineEventKind::Closed => format!("  {at}  closed"),
        TimelineEventKind::Reopened => format!("  {at}  reopened"),
        TimelineEventKind::Merged { commit } => commit.as_deref().map_or_else(
            || format!("  {at}  merged"),
            |commit| format!("  {at}  merged  @{}", short_forge_id(commit)),
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;
    use crate::cli::Exit;
    use crate::forge::fake::FakeForge;
    use crate::forge::{DiffTotals, PullRequest};
    use crate::ids::BranchName;

    fn entry() -> crate::config::RepoEntry {
        crate::config::RepoEntry::new(String::new(), String::new())
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
        let fork = Fork::at("demo", &entry, Path::new(""));
        let request = Request {
            fork: &fork,
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
        let fork = Fork::at("demo", &entry, Path::new(""));
        let request = Request {
            fork: &fork,
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
    fn an_open_pull_with_an_answered_incident_exits_findings_even_with_a_persist_note() {
        let report = Report {
            repo: "demo".to_owned(),
            number: 7,
            state: "OPEN".to_owned(),
            branch: "feat/a".to_owned(),
            base: Some("main".to_owned()),
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            review: String::new(),
            updated: "2026-08-30T00:00:00Z".to_owned(),
            url: String::new(),
            is_draft: false,
            mergeable: Some("MERGEABLE".to_owned()),
            diff: Some(DiffTotals::default()),
            head_ref_deleted: Some(false),
            tip_commit_empty: Some(false),
            timeline: None,
            notes: vec!["could not persist forge snapshot: disk full".to_owned()],
        };

        assert_eq!(exit_for(&report), Exit::Findings);
        assert!(render(&report).contains("note: could not persist forge snapshot: disk full"));
        assert_eq!(
            serde_json::to_value(&report)
                .expect("serialize")
                .get("notes")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn render_keeps_the_pull_requests_present_state_context_together() {
        let report = Report {
            repo: "demo".to_owned(),
            number: 4545,
            state: "CLOSED".to_owned(),
            branch: "feat/egress-guard".to_owned(),
            base: Some("main".to_owned()),
            head: "ab12cd34ef56dead".to_owned(),
            review: "APPROVED".to_owned(),
            updated: "2026-08-30T00:00:00Z".to_owned(),
            url: "https://example.test/demo/pull/4545".to_owned(),
            is_draft: true,
            mergeable: Some("MERGEABLE".to_owned()),
            diff: Some(DiffTotals::default()),
            head_ref_deleted: Some(true),
            tip_commit_empty: Some(true),
            timeline: None,
            notes: Vec::new(),
        };

        assert_eq!(
            render(&report),
            "demo#4545  CLOSED (draft)  feat/egress-guard -> main  @ab12cd34ef56  \
             review APPROVED  updated 2026-08-30T00:00:00Z  [empty-diff]  \
             [deleted-head-ref]  [empty-tip-commit]  https://example.test/demo/pull/4545"
        );
    }

    #[test]
    fn render_escapes_every_control_character_the_forge_answered() {
        // A head branch, a state, a review decision, a url, the forge's own
        // timestamp text and a timeline event's time are its answer, not
        // this process's text.
        let report = Report {
            repo: "demo".to_owned(),
            number: 4545,
            state: "CLO\u{1b}[2JSED".to_owned(),
            branch: "feat/\u{7}egress".to_owned(),
            base: Some("ma\u{9b}in".to_owned()),
            head: "ab12\u{1b}cd34ef56dead".to_owned(),
            review: "APPRO\u{1b}]0;pwned\u{7}VED".to_owned(),
            updated: "2026-08-30\u{1b}[31m".to_owned(),
            url: "https://example.test/\u{1b}[2J".to_owned(),
            is_draft: false,
            mergeable: None,
            diff: None,
            head_ref_deleted: None,
            tip_commit_empty: None,
            timeline: Some(vec![crate::forge::TimelineEvent {
                at: "2026-08-30\u{1b}[0m".to_owned(),
                kind: crate::forge::TimelineEventKind::Closed,
            }]),
            notes: vec!["could not persist: \u{1b}[2J".to_owned()],
        };

        let text = render(&report);

        assert!(!text.contains('\u{1b}'), "was: {text:?}");
        assert!(!text.contains('\u{7}'), "was: {text:?}");
        assert!(!text.contains('\u{9b}'), "was: {text:?}");
        assert!(
            text.contains("review APPRO\u{fffd}]0;pwned\u{fffd}VED"),
            "was: {text:?}"
        );
        assert!(
            text.contains("feat/\u{fffd}egress -> ma\u{fffd}in"),
            "was: {text:?}"
        );
        assert!(text.contains("@ab12\u{fffd}cd34ef5"), "was: {text:?}");
    }

    #[test]
    fn render_shows_force_push_trees_and_marks_content_identical_rewrites() {
        let report = Report {
            repo: "demo".to_owned(),
            number: 7,
            state: "OPEN".to_owned(),
            branch: "feat/a".to_owned(),
            base: Some("main".to_owned()),
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            review: String::new(),
            updated: "2026-08-30T22:41:02Z".to_owned(),
            url: String::new(),
            is_draft: false,
            mergeable: Some("MERGEABLE".to_owned()),
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
            notes: Vec::new(),
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
            base: Some("main".to_owned()),
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            review: String::new(),
            updated: "2026-08-30T22:41:02Z".to_owned(),
            url: String::new(),
            is_draft: false,
            mergeable: Some("MERGEABLE".to_owned()),
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
            notes: Vec::new(),
        };

        assert!(
            !render(&report).contains("[same tree]"),
            "unavailable tree ids cannot prove a content-identical rewrite"
        );
    }
}
