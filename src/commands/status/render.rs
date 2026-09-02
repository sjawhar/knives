use std::fmt::Write as _;

use super::{
    BranchRow, BranchState, FindingGroup, LastNotch, PushRelation, RepoNotches, Report, SeenWindow,
    short,
};

fn tip_cell(row: &BranchRow) -> String {
    row.tip.clone().unwrap_or_else(|| {
        if row.state == BranchState::Divergent {
            "divergent".to_owned()
        } else {
            "-".to_owned()
        }
    })
}

fn origin_relation_cell(origin: &str, relation: &str) -> String {
    format!("origin={origin} ({relation})")
}

fn push_cell(row: &BranchRow) -> String {
    match row.push.as_ref() {
        None => "pushed".to_owned(),
        Some(PushRelation::Unpushed) => "unpushed".to_owned(),
        Some(PushRelation::UnpushedCommits) => "unpushed-commits".to_owned(),
        Some(PushRelation::Behind(origin)) => origin_relation_cell(origin, "behind"),
        Some(PushRelation::Diverged(origin)) => origin_relation_cell(origin, "diverged"),
        Some(PushRelation::Unresolved(origin)) => origin_relation_cell(origin, "unresolved"),
    }
}

/// `#61 (activity 3h)`: the number, anything unusual about it, and how long ago
/// it last moved. A comment-only review or a maintainer's question leaves the
/// review decision empty, so without the age a row can read unchanged for days
/// while the conversation on it moved on.
fn pull_cell(row: &BranchRow) -> String {
    let Some(pull) = &row.pr else {
        return "-".to_owned();
    };
    let mut details = vec![format!("#{}", pull.number)];
    if pull.state != "open" && pull.state != "unknown" {
        details.push(pull.state.clone());
    }
    if pull.draft {
        details.push("draft".to_owned());
    }
    if pull.stated == Some(true) {
        details.push("(stated)".to_owned());
    }
    if pull.state == "open"
        && let Some(age) = pull
            .activity_at
            .as_deref()
            .and_then(|at| crate::ledger::age(at, jiff::Timestamp::now()))
    {
        details.push(format!("(activity {age})"));
    }
    details.extend(
        pull.prior
            .iter()
            .map(|prior| format!("prior #{} {}", prior.number, prior.state)),
    );
    details.join(" ")
}

fn option_cell(value: Option<&String>) -> String {
    value.cloned().unwrap_or_else(|| "-".to_owned())
}

fn landed_cell(row: &BranchRow) -> String {
    row.landed
        .map_or_else(|| "-".to_owned(), |verdict| verdict.to_string())
}

fn claim_cell(row: &BranchRow) -> String {
    row.claim.as_ref().map_or_else(
        || "-".to_owned(),
        |claim| format!("{}/{}", short(&claim.id), owner_kind_name(claim.kind)),
    )
}

const fn owner_kind_name(kind: crate::store::OwnerKind) -> &'static str {
    match kind {
        crate::store::OwnerKind::HarnessSession => "harness-session",
        crate::store::OwnerKind::WorkspaceDerived => "workspace-derived",
        crate::store::OwnerKind::OsUser => "os-user",
    }
}

fn seen_cell(row: &BranchRow) -> String {
    if let Some(last_seen) = &row.last_seen {
        return crate::ledger::age(last_seen, jiff::Timestamp::now())
            .unwrap_or_else(|| last_seen.clone());
    }
    match row.seen {
        Some(SeenWindow::NoneSinceClaim) => "none-since-claim".to_owned(),
        Some(SeenWindow::NoneWithinWindow) => "none-within-window".to_owned(),
        None => "-".to_owned(),
    }
}

/// How much of a notch's text a branch line carries.
const NOTCH_TEXT: usize = 32;

/// `"text…" (3d @1a2b3c4d5e6f)`: the entry, its age, and the tip it was written
/// against. The anchor is what lets a reader tell a note that still describes
/// this branch from one that described an earlier tip.
fn notch_summary(notch: &LastNotch) -> String {
    let mut text = notch.text.clone();
    if let Some(disposition) = &notch.disposition {
        text = format!("{}:{text}", crate::ledger::inline_human_text(disposition));
    }
    let escaped = crate::ledger::inline_human_text(&text);
    let mut shown: String = escaped.chars().take(NOTCH_TEXT).collect();
    if escaped.chars().count() > NOTCH_TEXT {
        shown.push('…');
    }
    let anchor = notch
        .anchor
        .as_deref()
        .map_or_else(String::new, |anchor| format!(" @{anchor}"));
    let summary = crate::ledger::age(&notch.ts, jiff::Timestamp::now()).map_or_else(
        || format!("\"{shown}\"{anchor}"),
        |age| format!("\"{shown}\" ({age}{anchor})"),
    );
    if notch.count > 1 {
        format!("{summary}+{}", notch.count - 1)
    } else {
        summary
    }
}

fn notch_cell(row: &BranchRow) -> String {
    row.notch
        .as_ref()
        .map_or_else(|| "-".to_owned(), notch_summary)
}

fn repo_notch_line(notches: &RepoNotches) -> String {
    format!(
        "  notches  {} repo-level, newest: {}",
        notches.count,
        notch_summary(&notches.last)
    )
}

fn branch_table(rows: &[BranchRow]) -> Vec<String> {
    const HEADER: [&str; 11] = [
        "branch", "state", "tip", "push", "pr", "review", "checks", "landed", "claim", "seen",
        "notch",
    ];
    let cells: Vec<[String; 11]> = rows
        .iter()
        .map(|row| {
            [
                row.name.to_string(),
                row.state.to_string(),
                tip_cell(row),
                push_cell(row),
                pull_cell(row),
                option_cell(row.review.as_ref()),
                option_cell(row.checks.as_ref()),
                landed_cell(row),
                claim_cell(row),
                seen_cell(row),
                notch_cell(row),
            ]
        })
        .collect();
    let mut widths = HEADER.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let format_row = |cells: [&str; 11]| {
        let mut line = String::from("    ");
        for (index, (cell, width)) in cells.into_iter().zip(widths).enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            let _ = write!(line, "{cell:<width$}");
        }
        line.trim_end().to_owned()
    };
    let mut lines = vec![format_row(HEADER)];
    lines.extend(
        cells
            .iter()
            .map(|row| format_row(row.each_ref().map(String::as_str))),
    );
    lines
}

/// How many subjects the one-line-per-kind view names before `and N more`.
const SUBJECTS_PER_LINE: usize = 8;

fn finding_lines(groups: &[FindingGroup], verbose: bool) -> Vec<String> {
    if verbose {
        return groups
            .iter()
            .flat_map(|group| {
                group
                    .subjects
                    .iter()
                    .zip(
                        group
                            .details
                            .iter()
                            .map(String::as_str)
                            .chain(std::iter::repeat("")),
                    )
                    .map(|(subject, detail)| {
                        if detail.is_empty() {
                            format!("    {}  {subject}", group.kind)
                        } else {
                            format!("    {}  {subject}: {detail}", group.kind)
                        }
                    })
            })
            .collect();
    }
    let width = groups
        .iter()
        .map(|group| group.kind.to_string().len())
        .max()
        .unwrap_or(0);
    groups
        .iter()
        .map(|group| {
            let shown: Vec<&str> = group
                .subjects
                .iter()
                .take(SUBJECTS_PER_LINE)
                .map(String::as_str)
                .collect();
            let mut subjects = shown.join(", ");
            let hidden = group.count.saturating_sub(shown.len());
            if hidden > 0 {
                subjects = format!("{subjects}, and {hidden} more");
            }
            format!(
                "    {:<width$}  {:>3}  {subjects}",
                group.kind,
                group.count,
                width = width
            )
        })
        .collect()
}

pub fn render(report: &Report, verbose: bool) -> String {
    let release = report.newest_release.as_deref().unwrap_or("none");
    let consulted = if report.forge.consulted {
        "consulted"
    } else {
        "not consulted"
    };
    let mut lines = vec![format!(
        "{}  trunk {}  release {release}  forge {consulted} ({}ms)",
        report.repo, report.trunk, report.forge.elapsed_ms
    )];
    if !report.problems.is_empty() {
        lines.push(format!("  UNANSWERED  {}", report.problems.len()));
        lines.extend(report.problems.iter().map(|problem| {
            format!(
                "    {}",
                problem.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        }));
    }
    lines.push(format!("  branches    {}", report.branches.len()));
    if !report.branches.is_empty() {
        lines.extend(branch_table(&report.branches));
    }
    lines.push(format!("  findings    {}", report.findings.len()));
    lines.extend(finding_lines(&report.findings, verbose));
    if let Some(notches) = &report.repo_notches {
        lines.push(repo_notch_line(notches));
    }
    // Workspaces named for a branch row sit in that row's `workspace` cell;
    // this line is only what is left, which is why it never matches the length
    // of `jj workspace list`.
    if !report.other_workspaces.is_empty() {
        lines.push(format!(
            "  workspaces  {} not named for a branch above: {}",
            report.other_workspaces.len(),
            report.other_workspaces.join(", ")
        ));
    }
    if !report.notes.is_empty() {
        lines.push(format!("  notes       {}", report.notes.len()));
        lines.extend(report.notes.iter().map(|note| format!("    {note}")));
    }
    lines.join("\n")
}

#[cfg(test)]
pub(super) fn render_verbose(report: &Report) -> String {
    render(report, true)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;
    use crate::commands::status::{
        BranchState, ClaimCell, FindingGroup, LastNotch, PullCell, SeenWindow,
    };
    use crate::detect::{FindingKind, LandedVerdict};
    use crate::ids::BranchName;
    use crate::store::OwnerKind;

    fn new_row(name: &str) -> BranchRow {
        BranchRow {
            name: BranchName::new(name),
            state: BranchState::Unknown,
            tip: None,
            push: None,
            pr: None,
            review: None,
            checks: None,
            landed: None,
            flags: Vec::new(),
            claim: None,
            last_seen: None,
            seen: None,
            workspace: None,
            notch: None,
        }
    }

    fn report_with(row: BranchRow) -> Report {
        Report {
            repo: "demo".to_owned(),
            trunk: "main".to_owned(),
            branches: vec![row],
            ..Report::default()
        }
    }

    #[test]
    fn problems_render_before_the_branch_table() {
        let report = Report {
            repo: "demo".to_owned(),
            trunk: "main".to_owned(),
            problems: vec!["forge unavailable".to_owned()],
            branches: vec![new_row("feat/alpha")],
            ..Report::default()
        };

        let rendered = render(&report, false);

        assert!(
            rendered.find("UNANSWERED") < rendered.find("branches"),
            "was: {rendered}"
        );
    }

    #[test]
    fn a_claimed_row_carries_owner_kind_and_age() {
        let mut row = new_row("feat/alpha");
        row.claim = Some(ClaimCell {
            id: "abcdefghijklmnop".to_owned(),
            kind: OwnerKind::HarnessSession,
            since: "2026-08-01T00:00:00Z".to_owned(),
            why: "status model".to_owned(),
        });
        row.last_seen = Some(jiff::Timestamp::now().to_string());

        let rendered = render(&report_with(row), false);

        assert!(
            rendered.contains("abcdefghijkl/harness-session"),
            "was: {rendered}"
        );
        assert!(rendered.contains("now"), "was: {rendered}");
    }

    #[test]
    fn findings_render_one_line_per_kind_with_count() {
        let report = Report {
            repo: "demo".to_owned(),
            trunk: "main".to_owned(),
            findings: vec![FindingGroup {
                kind: FindingKind::ChecksFailing,
                count: 3,
                subjects: vec!["#11".to_owned(), "#12".to_owned()],
                details: vec![
                    "#11 has failing checks: build".to_owned(),
                    "#12 has failing checks: lint".to_owned(),
                ],
            }],
            ..Report::default()
        };

        let rendered = render(&report, false);

        let lines: Vec<_> = rendered
            .lines()
            .filter(|line| line.contains("checks-failing"))
            .collect();
        assert_eq!(lines.len(), 1, "was: {rendered}");
        assert!(lines[0].contains('3'), "was: {}", lines[0]);
        assert!(
            lines[0].contains("#11, #12, and 1 more"),
            "was: {}",
            lines[0]
        );
    }

    #[test]
    fn a_behind_row_renders_origin_and_relation() {
        let mut row = new_row("feat/alpha");
        row.push = Some(PushRelation::Behind("0123456789ab".to_owned()));

        let rendered = render(&report_with(row), false);

        assert!(
            rendered.contains("origin=0123456789ab (behind)"),
            "was: {rendered}"
        );
    }

    #[test]
    fn an_origin_relation_serializes_as_a_relation_and_tip() {
        let mut row = new_row("feat/alpha");
        row.push = Some(PushRelation::Behind("0123456789ab".to_owned()));

        let rendered = serde_json::to_value(report_with(row))
            .unwrap_or_else(|error| panic!("status report should serialize: {error}"));

        assert_eq!(rendered["branches"][0]["push"], "behind");
        assert_eq!(rendered["branches"][0]["origin_tip"], "0123456789ab");
    }

    #[test]
    fn a_covered_claim_renders_none_since_claim() {
        let mut row = new_row("feat/alpha");
        row.claim = Some(ClaimCell {
            id: "owner".to_owned(),
            kind: OwnerKind::OsUser,
            since: "2026-08-01T00:00:00Z".to_owned(),
            why: "status model".to_owned(),
        });
        row.seen = Some(SeenWindow::NoneSinceClaim);

        let rendered = render(&report_with(row), false);
        let line = rendered
            .lines()
            .find(|line| line.contains("feat/alpha"))
            .expect("branch line");
        assert_eq!(
            line.split_whitespace().nth(9),
            Some("none-since-claim"),
            "was: {line}"
        );
    }

    #[test]
    fn an_exhausted_window_renders_none_within_window() {
        let mut row = new_row("feat/alpha");
        row.claim = Some(ClaimCell {
            id: "owner".to_owned(),
            kind: OwnerKind::OsUser,
            since: "2026-08-01T00:00:00Z".to_owned(),
            why: "status model".to_owned(),
        });
        row.seen = Some(SeenWindow::NoneWithinWindow);

        let rendered = render(&report_with(row), false);
        let line = rendered
            .lines()
            .find(|line| line.contains("feat/alpha"))
            .expect("branch line");
        assert_eq!(
            line.split_whitespace().nth(9),
            Some("none-within-window"),
            "was: {line}"
        );
    }

    #[test]
    fn branch_rows_render_as_an_aligned_table_with_the_new_columns() {
        let mut full = new_row("feat/alpha");
        full.state = BranchState::Approved;
        full.tip = Some("0123456789ab".to_owned());
        full.pr = Some(PullCell {
            number: 1128,
            state: "open".to_owned(),
            draft: false,
            stated: None,
            activity_at: None,
            prior: Vec::new(),
        });
        full.review = Some("approved".to_owned());
        full.checks = Some("ok".to_owned());
        full.landed = Some(LandedVerdict::InTrunk);
        let lines = branch_table(&[full, new_row("fix/a-much-longer-branch-name")]);

        assert_eq!(lines.len(), 3, "was: {lines:?}");
        assert!(lines[0].contains("state") && lines[0].contains("claim"));
        assert!(lines[1].contains("#1128"), "was: {}", lines[1]);
        assert!(lines[2].contains("pushed"), "was: {}", lines[2]);
    }

    #[test]
    fn the_notch_cell_keeps_its_age_and_masked_count() {
        let mut row = new_row("feat/log-queue");
        row.notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "superseded by #1157".to_owned(),
            disposition: None,
            anchor: Some("1a2b3c4d5e6f".to_owned()),
            count: 2,
        });

        let rendered = render(&report_with(row), false);

        assert!(
            rendered.contains("\"superseded by #1157\" (now @1a2b3c4d5e6f)+1"),
            "was: {rendered}"
        );
    }

    #[test]
    fn verbose_findings_expand_subjects_without_detail_prose() {
        let report = Report {
            repo: "demo".to_owned(),
            trunk: "main".to_owned(),
            findings: vec![FindingGroup {
                kind: FindingKind::ChecksFailing,
                count: 2,
                subjects: vec!["#1".to_owned(), "#2".to_owned()],
                details: vec![
                    "#1 has failing checks".to_owned(),
                    "#2 has failing checks".to_owned(),
                ],
            }],
            ..Report::default()
        };

        let rendered = render_verbose(&report);

        assert!(rendered.contains("checks-failing  #1"), "was: {rendered}");
        assert!(rendered.contains("checks-failing  #2"), "was: {rendered}");
    }
}
