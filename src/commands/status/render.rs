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

fn origin_relation_cell(row: &BranchRow, relation: &str) -> String {
    let origin = row
        .origin_tip
        .as_deref()
        .expect("non-clean push has an origin tip");
    format!("origin={origin} ({relation})")
}

fn push_cell(row: &BranchRow) -> String {
    match row.push {
        None => "pushed".to_owned(),
        Some(PushRelation::Unpushed) => "unpushed".to_owned(),
        Some(PushRelation::UnpushedCommits) => "unpushed-commits".to_owned(),
        Some(PushRelation::Behind) => origin_relation_cell(row, "behind"),
        Some(PushRelation::Diverged) => origin_relation_cell(row, "diverged"),
        Some(PushRelation::Unresolved) => origin_relation_cell(row, "unresolved"),
    }
}

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
    let summary = crate::ledger::age(&notch.ts, jiff::Timestamp::now()).map_or_else(
        || format!("\"{shown}\""),
        |age| format!("\"{shown}\" ({age})"),
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
        for (index, cell) in cells.into_iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(&format!("{cell:<width$}", width = widths[index]));
        }
        line.trim_end().to_owned()
    };
    let mut lines = vec![format_row(HEADER)];
    lines.extend(cells.iter().map(|row| format_row(row.each_ref().map(String::as_str))));
    lines
}

fn finding_lines(groups: &[FindingGroup], verbose: bool) -> Vec<String> {
    if verbose {
        return groups
            .iter()
            .flat_map(|group| {
                group
                    .subjects
                    .iter()
                    .map(|subject| format!("    {}  {subject}", group.kind))
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
            let mut subjects = group.subjects.join(", ");
            let hidden = group.count.saturating_sub(group.subjects.len());
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
    if !report.other_workspaces.is_empty() {
        lines.push(format!("  workspaces  {}", report.other_workspaces.join(", ")));
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
            origin_tip: None,
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

        assert!(rendered.contains("abcdefghijkl/harness-session"), "was: {rendered}");
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
            }],
            ..Report::default()
        };

        let rendered = render(&report, false);

        let lines: Vec<_> = rendered
            .lines()
            .filter(|line| line.contains("checks-failing"))
            .collect();
        assert_eq!(lines.len(), 1, "was: {rendered}");
        assert!(lines[0].contains("3"), "was: {}", lines[0]);
        assert!(lines[0].contains("#11, #12, and 1 more"), "was: {}", lines[0]);
    }

    #[test]
    fn a_behind_row_renders_origin_and_relation() {
        let mut row = new_row("feat/alpha");
        row.push = Some(PushRelation::Behind);
        row.origin_tip = Some("0123456789ab".to_owned());

        let rendered = render(&report_with(row), false);

        assert!(
            rendered.contains("origin=0123456789ab (behind)"),
            "was: {rendered}"
        );
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
        assert_eq!(line.split_whitespace().nth(9), Some("none-since-claim"), "was: {line}");
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
            count: 2,
        });

        let rendered = render(&report_with(row), false);

        assert!(rendered.contains("\"superseded by #1157\" (now)+1"), "was: {rendered}");
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
            }],
            ..Report::default()
        };

        let rendered = render_verbose(&report);

        assert!(rendered.contains("checks-failing  #1"), "was: {rendered}");
        assert!(rendered.contains("checks-failing  #2"), "was: {rendered}");
    }
}
