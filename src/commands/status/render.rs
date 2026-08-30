use super::{
    BTreeMap, BranchRow, Finding, FindingKind, LastNotch, OriginRelation, RepoNotches, Report,
    StatedPull, short,
};
/// One line per kind of finding, naming every subject.
///
/// A finding per branch times a detector per finding made the report unreadable: one
/// repository printed 89 blocks, and a wall of text that has to be read in full to
/// find the two things that matter is the same as not being told. Nothing is dropped
/// here, only folded: every subject is named, and `--verbose` still prints each
/// finding with its own detail line.
pub(super) fn grouped(findings: &[Finding]) -> Vec<String> {
    let mut order: Vec<FindingKind> = Vec::new();
    let mut by_kind: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for finding in findings {
        let key = finding.kind.to_string();
        if !order.iter().any(|kind| kind.to_string() == key) {
            order.push(finding.kind);
        }
        by_kind
            .entry(key)
            .or_default()
            .push(finding.subject.short());
    }
    let width = order.iter().map(|k| k.to_string().len()).max().unwrap_or(0);
    order
        .iter()
        .filter_map(|kind| {
            let key = kind.to_string();
            let subjects = by_kind.get(&key)?;
            // Enough subjects to act on, then a count, so one loud detector cannot push
            // the others off the screen.
            let shown = subjects
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let rest = subjects.len().saturating_sub(6);
            let listed = if rest == 0 {
                shown
            } else {
                format!("{shown}, and {rest} more")
            };
            Some(format!(
                "    {key:<width$}  {:>3}  {listed}",
                subjects.len()
            ))
        })
        .collect()
}

/// Active claims, one block each.
pub(super) fn claim_lines(claims: &[crate::store::Claim]) -> Vec<String> {
    if claims.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("  claims      {}", claims.len())];
    for claim in claims {
        lines.push(format!(
            "    {}  {}  since {}",
            claim.branch, claim.owner, claim.started
        ));
        lines.push(format!("      {}", claim.why));
    }
    lines
}

fn branch_cell(row: &BranchRow) -> String {
    row.name.to_string()
}

fn tip_cell(row: &BranchRow) -> String {
    row.tip
        .as_ref()
        .map_or_else(|| "divergent".to_owned(), |tip| short(tip.as_str()))
}

fn push_cell(row: &BranchRow) -> String {
    match (&row.origin_tip, &row.tip) {
        (None, _) => "unpushed".to_owned(),
        (Some(origin), Some(tip)) if origin != tip => match row.origin_relation {
            Some(OriginRelation::Ahead) => "unpushed-commits".to_owned(),
            Some(OriginRelation::Behind) => format!("origin={} (behind)", short(origin.as_str())),
            Some(OriginRelation::Diverged) => {
                format!("origin={} (diverged)", short(origin.as_str()))
            }
            None => format!("origin={} (unresolved)", short(origin.as_str())),
        },
        (Some(_), _) => "pushed".to_owned(),
    }
}

fn stated_pull_cell(stated: &StatedPull) -> String {
    format!("#{} {}", stated.number, stated_pull_details(stated))
}

fn stated_pull_details(stated: &StatedPull) -> String {
    format!("{} (stated)", stated.state.to_lowercase())
}

fn pull_request_cell(row: &BranchRow, forge_consulted: bool) -> String {
    row.pull_request.as_ref().map_or_else(
        || {
            if !forge_consulted {
                return row.stated_pull.as_ref().map_or_else(
                    || "unknown (forge unavailable)".to_owned(),
                    stated_pull_cell,
                );
            }
            row.stated_pull
                .as_ref()
                .map_or_else(|| "no-pr".to_owned(), stated_pull_cell)
        },
        |pr| {
            let mut details = vec![format!("#{}", pr.number)];
            if !pr.is_open() {
                details.push(pr.state.to_lowercase());
            }
            if pr.is_draft {
                details.push("draft".to_owned());
            }
            if let Some(stated) = &row.stated_pull {
                if stated.number == pr.number {
                    details.push(stated_pull_details(stated));
                } else {
                    details.push(stated_pull_cell(stated));
                }
            }
            for prior in &row.prior_pulls {
                details.push(format!(
                    "prior #{} {}",
                    prior.number,
                    prior.state.to_lowercase()
                ));
            }
            details.join(" ")
        },
    )
}

fn review_cell(row: &BranchRow) -> String {
    match &row.pull_request {
        Some(pr) if pr.review_decision.is_empty() => "no-review".to_owned(),
        Some(pr) => pr.review_decision.clone(),
        None => "-".to_owned(),
    }
}

fn checks_cell(row: &BranchRow) -> String {
    match row.pull_request.as_ref() {
        Some(pr) if pr.is_open() => match row.checks.as_ref() {
            Some(checks) if checks.failing() => "failing".to_owned(),
            Some(checks) if !checks.ran() => "none-ran".to_owned(),
            Some(checks) if checks.pending() => "pending".to_owned(),
            Some(_) => "ok".to_owned(),
            None => "-".to_owned(),
        },
        Some(_) | None => "-".to_owned(),
    }
}

fn landed_cell(row: &BranchRow) -> String {
    row.landed
        .map_or_else(|| "-".to_owned(), |verdict| verdict.to_string())
}

fn flags_cell(row: &BranchRow) -> String {
    let mut flags = Vec::new();
    if let Some(pr) = &row.pull_request {
        if pr.conflicting() {
            flags.push("CONFLICTING");
        } else if pr.merge_state_status.eq_ignore_ascii_case("BEHIND") {
            flags.push("behind-base");
        }
    }
    if row.review_stale == Some(true) {
        flags.push("review-stale");
    }
    if row.fork_only {
        flags.push("fork-only");
    }
    if flags.is_empty() {
        "-".to_owned()
    } else {
        flags.join(",")
    }
}

/// How much of a notch's text a branch line carries.
const NOTCH_TEXT: usize = 32;

/// Render a ledger entry in the one-line status form.
fn notch_summary(notch: &LastNotch) -> String {
    let collapsed = notch.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped = crate::ledger::inline_human_text(&collapsed);
    let mut shown: String = escaped.chars().take(NOTCH_TEXT).collect();
    if escaped.chars().count() > NOTCH_TEXT {
        shown.push('…');
    }
    crate::ledger::age(&notch.ts, jiff::Timestamp::now()).map_or_else(
        || format!("\"{shown}\""),
        |age| format!("\"{shown}\" ({age})"),
    )
}

/// The newest notch on this branch, as one token.
///
/// Truncated and whitespace-collapsed because an entry's text is free prose that
/// may run to a paragraph and may contain newlines, and this is a table cell: one
/// stray newline destroys every column below it.
fn notch_cell(row: &BranchRow) -> String {
    row.last_notch
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

fn branch_table(rows: &[BranchRow], forge_consulted: bool) -> Vec<String> {
    const HEADER: [&str; 9] = [
        "branch", "tip", "push", "pr", "review", "checks", "landed", "flags", "notch",
    ];

    let cells: Vec<[String; 9]> = rows
        .iter()
        .map(|row| {
            [
                branch_cell(row),
                tip_cell(row),
                push_cell(row),
                pull_request_cell(row, forge_consulted),
                review_cell(row),
                checks_cell(row),
                landed_cell(row),
                flags_cell(row),
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
    let format_row = |cells: [&str; 9]| {
        let [
            branch,
            tip,
            push,
            pull_request,
            review,
            checks,
            landed,
            flags,
            notch,
        ] = cells;
        let [
            branch_width,
            tip_width,
            push_width,
            pull_request_width,
            review_width,
            checks_width,
            landed_width,
            flags_width,
            notch_width,
        ] = widths;
        format!(
            "    {branch:<branch_width$}  {tip:<tip_width$}  {push:<push_width$}  {pull_request:<pull_request_width$}  {review:<review_width$}  {checks:<checks_width$}  {landed:<landed_width$}  {flags:<flags_width$}  {notch:<notch_width$}"
        )
        .trim_end()
        .to_owned()
    };
    let mut lines = vec![format_row(HEADER)];
    lines.extend(cells.iter().map(|row| {
        let [
            branch,
            tip,
            push,
            pull_request,
            review,
            checks,
            landed,
            flags,
            notch,
        ] = row.each_ref();
        format_row([
            branch.as_str(),
            tip.as_str(),
            push.as_str(),
            pull_request.as_str(),
            review.as_str(),
            checks.as_str(),
            landed.as_str(),
            flags.as_str(),
            notch.as_str(),
        ])
    }));
    lines
}

pub fn render(report: &Report, verbose: bool) -> String {
    // The repository is named once, at the top, and everything under it is indented.
    // Prefixing every section with it repeated the name four times per repo, which over
    // ten repos is forty lines of the same word and no structure at all.
    let mut lines: Vec<String> = vec![report.repo.clone()];
    if !report.releases.is_empty() {
        lines.push(format!(
            "  releases    {} checked: {}",
            report.releases.len(),
            report.releases.join(", ")
        ));
    }
    if report.branches.is_empty() {
        lines.push("  branches    none".to_owned());
    } else {
        lines.push(format!("  branches    {}", report.branches.len()));
    }
    if let Some(notches) = &report.repo_notches {
        lines.push(repo_notch_line(notches));
    }
    if !report.branches.is_empty() {
        lines.extend(branch_table(&report.branches, report.forge_consulted));
    }
    if report.findings.is_empty() {
        lines.push("  findings    none".to_owned());
    } else {
        lines.push(format!("  findings    {}", report.findings.len()));
        if verbose {
            for finding in &report.findings {
                lines.push(format!(
                    "    [{}] {}",
                    finding.kind,
                    finding.subject.short()
                ));
                lines.push(format!("      {}", finding.detail));
            }
        } else {
            lines.extend(grouped(&report.findings));
        }
    }
    // Problems decide the exit code, so printing them is not optional: a non-zero
    // exit whose reason appears nowhere in the output is a gate nobody can act on.
    if !report.problems.is_empty() {
        lines.push(format!("  unanswered  {}", report.problems.len()));
        for problem in &report.problems {
            // One physical line per problem: a forge error can carry its own
            // newlines ("Try authenticating with: gh auth login"), and a spilled
            // continuation at column zero reads as a second, unlabeled problem.
            let one_line: Vec<&str> = problem.split_whitespace().collect();
            lines.push(format!("    {}", one_line.join(" ")));
        }
    }
    lines.extend(claim_lines(&report.claims));
    let mut notes: Vec<String> = report.notes.clone();
    if !report.forge_consulted {
        notes.push("pull request state was not checked; branch columns are unknown".to_owned());
    }
    if !notes.is_empty() {
        lines.push(format!("  notes       {}", notes.len()));
        for note in &notes {
            lines.push(format!("    {note}"));
        }
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
    use super::super::test_fixtures::{pull_request, row};
    use super::super::{PriorPull, branch_findings};
    use super::*;
    use crate::detect::LandedVerdict;
    use crate::ids::CommitId;
    #[test]
    fn a_missing_forge_renders_unknown_not_no_pr() {
        // A missing snapshot means the forge did not answer; `no-pr` would claim
        // it did answer and found nothing.
        let row = row("feat/alpha", None, None);

        assert_eq!(
            pull_request_cell(&row, false),
            "unknown (forge unavailable)"
        );
        assert_eq!(pull_request_cell(&row, true), "no-pr");
    }
    #[test]
    fn branch_rows_render_as_an_aligned_table_with_a_header() {
        // Vertical alignment without horizontal alignment made ten-branch reports
        // unreadable: every fact was present and nothing lined up.
        let with_pr = row(
            "feat/alpha",
            Some(LandedVerdict::InTrunk),
            Some(pull_request(1128)),
        );
        let bare = row("fix/a-much-longer-branch-name", None, None);

        let lines = branch_table(&[with_pr, bare], true);

        assert_eq!(lines.len(), 3, "header plus one row per branch: {lines:?}");
        let header = &lines[0];
        assert!(header.contains("branch") && header.contains("pr") && header.contains("landed"));
        // Every row starts each column at the same offset as the header.
        let column_start = |line: &str, word: &str| line.find(word).unwrap_or(usize::MAX);
        let tip_at = column_start(header, "tip");
        for line in &lines[1..] {
            assert!(line.len() >= tip_at, "short row breaks alignment: {line:?}");
        }
        assert!(lines[1].contains("#1128"), "was: {}", lines[1]);
        assert!(lines[1].contains("APPROVED"));
        assert!(lines[2].contains("no-pr"));
        assert!(
            lines[2].contains('-'),
            "empty cells render as placeholders, not gaps"
        );
    }

    /// The offset at which each column's content begins, and the gap that precedes it.
    ///
    /// Cells never contain two consecutive spaces, so a run of two or more spaces is
    /// unambiguously a column separator plus that column's padding.
    fn columns(line: &str) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut gap = 0;
        for (offset, ch) in line.char_indices() {
            if ch == ' ' {
                gap += 1;
            } else {
                if gap >= 2 || offset == 0 {
                    found.push((offset, gap));
                }
                gap = 0;
            }
        }
        found
    }

    #[test]
    fn an_empty_cell_never_shifts_its_neighbours() {
        let with_flags = {
            let mut pr = pull_request(7);
            pr.mergeable = "CONFLICTING".to_owned();
            row("feat/conflicted", None, Some(pr))
        };
        let plain = row("feat/plain", None, None);

        let lines = branch_table(&[with_flags, plain], true);

        let header_columns = columns(&lines[0]);
        let header_offsets: Vec<usize> = header_columns.iter().map(|(offset, _)| *offset).collect();
        assert_eq!(header_offsets.len(), 9, "was: {}", lines[0]);
        for line in &lines {
            assert_eq!(
                line.chars().take_while(|ch| *ch == ' ').count(),
                4,
                "was: {line}"
            );
            let row_columns = columns(line);
            let row_offsets: Vec<usize> = row_columns.iter().map(|(offset, _)| *offset).collect();
            assert_eq!(row_offsets, header_offsets, "was: {line}");
            assert_eq!(
                row_columns
                    .iter()
                    .skip(1)
                    .map(|(_, gap)| *gap)
                    .min()
                    .expect("a table row has separators"),
                2,
                "was: {line}"
            );
        }
        assert_eq!(columns(&lines[2]).len(), 9, "was: {}", lines[2]);
        assert!(lines[2].ends_with(" -"), "was: {}", lines[2]);
        assert!(lines[1].contains("CONFLICTING"));
    }
    #[test]
    fn a_branchs_newest_notch_is_one_token_at_the_end_of_its_line() {
        // Status text is already dense: the breadcrumb is one token, and its
        // legibility overhaul is separate work.
        let mut row = row("feat/log-queue", None, None);
        row.last_notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "superseded by #1157".to_owned(),
        });
        let lines = branch_table(&[row], true);
        assert!(lines[0].contains("notch"), "header: {}", lines[0]);
        assert!(
            lines[1].ends_with("\"superseded by #1157\" (now)"),
            "was: {}",
            lines[1]
        );
    }
    #[test]
    fn a_long_or_multi_line_notch_cannot_break_the_table() {
        // An entry's text is free prose that may run to a paragraph and may carry
        // newlines. One stray newline destroys every column below it.
        let mut row = row("feat/alpha", None, None);
        row.last_notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "parked by the owner\nuntil the trait lands upstream, which may be weeks"
                .to_owned(),
        });
        let lines = branch_table(&[row], true);
        assert_eq!(lines.len(), 2, "was: {lines:?}");
        assert!(!lines[1].contains('\n'));
        assert!(lines[1].contains('…'), "truncation is marked: {}", lines[1]);
        assert!(
            lines[1].contains("parked by the owner until"),
            "newlines collapse to spaces: {}",
            lines[1]
        );
    }
    #[test]
    fn a_notch_control_character_cannot_reach_the_status_table() {
        let mut row = row("feat/alpha", None, None);
        row.last_notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "parked\u{1b}now\ragain".to_owned(),
        });

        let lines = branch_table(&[row], true);
        assert!(!lines[1].contains('\u{1b}'), "was: {:?}", lines[1]);
        assert!(!lines[1].contains('\r'), "was: {:?}", lines[1]);
        assert!(lines[1].contains('\u{fffd}'), "was: {:?}", lines[1]);
    }
    #[test]
    fn a_branch_with_no_notch_renders_the_empty_placeholder() {
        let lines = branch_table(&[row("feat/alpha", None, None)], true);
        assert!(lines[1].ends_with(" -"), "was: {}", lines[1]);
        assert_eq!(columns(&lines[1]).len(), 9, "was: {}", lines[1]);
    }
    #[test]
    fn a_problem_is_printed_not_just_counted_in_the_exit_code() {
        // Problems drive Exit::Incomplete. A non-zero exit whose cause appears
        // nowhere in the output cannot be acted on.
        let report = Report {
            repo: "demo".to_owned(),
            problems: vec!["cannot tell whether feat/x landed".to_owned()],
            ..Report::default()
        };
        let out = render(&report, true);
        assert!(
            out.contains("cannot tell whether feat/x landed"),
            "was: {out}"
        );
        assert!(out.contains("unanswered"), "was: {out}");
    }
    #[test]
    fn a_problem_carrying_newlines_renders_as_one_physical_line() {
        // Given: a problem embedding a forge error with its own remediation line
        let report = Report {
            repo: "demo".to_owned(),
            problems: vec![
                "pull request state unavailable: HTTP 401\nTry authenticating with: gh auth login"
                    .to_owned(),
            ],
            ..Report::default()
        };
        // When: the report renders as text
        let out = render(&report, true);
        // Then: the problem occupies one indented line, not a spilled continuation
        let problem_lines: Vec<&str> = out
            .lines()
            .filter(|line| line.contains("HTTP 401") || line.contains("Try authenticating"))
            .collect();
        assert_eq!(problem_lines.len(), 1, "was: {out}");
        assert!(
            problem_lines[0].contains("HTTP 401 Try authenticating with: gh auth login"),
            "was: {out}"
        );
    }
    #[test]
    fn ci_readiness_cells_preserve_draft_and_check_facts() {
        // Given: a draft with red CI and a draft whose checks have not run
        let mut failing = pull_request(11);
        failing.is_draft = true;
        let mut failing = row("feat/failing", None, Some(failing));
        failing.checks = Some(crate::forge::ChecksSummary {
            runs: vec![crate::forge::CheckRun {
                name: "build".to_owned(),
                conclusion: Some("FAILURE".to_owned()),
            }],
        });
        let mut never_ran = pull_request(12);
        never_ran.is_draft = true;
        let mut never_ran = row("feat/never-ran", None, Some(never_ran));
        never_ran.checks = Some(crate::forge::ChecksSummary::default());
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![failing, never_ran],
            ..Report::default()
        };

        // When: the branch rows are rendered
        let rendered = render(&report, false);
        let failing_line = rendered
            .lines()
            .find(|line| line.contains("feat/failing"))
            .expect("the failing branch line");
        let never_ran_line = rendered
            .lines()
            .find(|line| line.contains("feat/never-ran"))
            .expect("the never-ran branch line");

        // Then: each row retains its draft and CI facts in separate table cells
        assert!(
            failing_line.contains("draft") && failing_line.contains("failing"),
            "was: {failing_line}"
        );
        assert!(
            never_ran_line.contains("draft") && never_ran_line.contains("none-ran"),
            "was: {never_ran_line}"
        );
    }
    #[test]
    fn pending_checks_are_not_rendered_as_ok() {
        let mut pending = row("feat/pending", None, Some(pull_request(13)));
        pending.checks = Some(crate::forge::ChecksSummary {
            runs: vec![crate::forge::CheckRun {
                name: "build".to_owned(),
                conclusion: None,
            }],
        });

        assert_eq!(checks_cell(&pending), "pending");
    }
    #[test]
    fn a_pending_legacy_status_context_renders_as_pending() {
        let facts = crate::forge::github::parse_pull_facts(
            r#"{"data":{"repository":{"p13":{"number":13,"state":"OPEN","headRefName":"feat/pending",
            "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":{"contexts":{"nodes":[{"__typename":"StatusContext",
            "context":"legacy-ci","state":"PENDING"}]}}}}]}}}}}"#,
            &[13],
        )
        .expect("facts parse");
        let fact = &facts[&13];
        let mut pending = row("feat/pending", None, Some(fact.pull.clone()));
        pending.checks = fact.details.checks.clone();
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![pending],
            ..Report::default()
        };

        let rendered = render(&report, false);
        let pending_line = rendered
            .lines()
            .find(|line| line.contains("feat/pending"))
            .expect("the pending branch line");

        assert_eq!(
            pending_line.split_whitespace().nth(5),
            Some("pending"),
            "was: {pending_line}"
        );
    }
    #[test]
    fn not_consulted_checks_do_not_render_as_none_ran() {
        // Given: matching pull requests whose checks were and were not consulted
        let mut no_checks = row("feat/no-checks", None, Some(pull_request(11)));
        no_checks.checks = Some(crate::forge::ChecksSummary::default());
        let not_consulted = row("feat/not-consulted", None, Some(pull_request(12)));
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![no_checks, not_consulted],
            ..Report::default()
        };

        // When: the branch rows are rendered
        let rendered = render(&report, false);
        let no_checks_line = rendered
            .lines()
            .find(|line| line.contains("feat/no-checks"))
            .expect("the consulted branch line");
        let not_consulted_line = rendered
            .lines()
            .find(|line| line.contains("feat/not-consulted"))
            .expect("the unconsulted branch line");

        // Then: the three states stay distinct
        assert!(no_checks_line.contains("none-ran"), "was: {no_checks_line}");
        assert!(
            !not_consulted_line.contains("none-ran"),
            "not consulted is not nothing-ran: {not_consulted_line}"
        );
        assert!(
            !not_consulted_line.contains("failing"),
            "not consulted is not failing: {not_consulted_line}"
        );
    }
    #[test]
    fn settled_pull_requests_do_not_report_obsolete_check_status() {
        // Given: a closed pull request whose recorded check rollup is red
        let mut pull_request = pull_request(4634);
        pull_request.state = "CLOSED".to_owned();
        let mut closed = row("feat/closed", None, Some(pull_request));
        closed.checks = Some(crate::forge::ChecksSummary {
            runs: vec![crate::forge::CheckRun {
                name: "build".to_owned(),
                conclusion: Some("FAILURE".to_owned()),
            }],
        });

        // When: the settled branch is rendered and analysed
        let rendered = branch_table(std::slice::from_ref(&closed), true).join("\n");
        let findings = branch_findings(&[closed]);

        // Then: no action-oriented CI token or finding is emitted
        assert!(!rendered.contains("none-ran"), "was: {rendered}");
        assert!(!rendered.contains("failing"), "was: {rendered}");
        assert!(
            !findings
                .iter()
                .any(|finding| finding.kind == FindingKind::ChecksFailing),
            "was: {findings:?}"
        );
    }
    #[test]
    fn a_draft_pull_request_says_so() {
        // Already requested from the forge and already deserialised, and nothing rendered it,
        // so the cheapest "not ready" signal there is was being thrown away.
        let mut pr = pull_request(7);
        pr.is_draft = true;
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![row("feat/alpha", None, Some(pr))],
            ..Report::default()
        };

        assert!(
            render(&report, false).contains("draft"),
            "was: {}",
            render(&report, false)
        );
    }
    #[test]
    fn the_same_inferred_and_stated_pull_number_is_rendered_once_with_its_provenance() {
        // Given: an open inferred pull request and a stated record for that same pull request.
        let mut row = row("feat/alpha", None, Some(pull_request(106)));
        row.stated_pull = Some(StatedPull {
            number: 106,
            state: "OPEN".to_owned(),
        });

        // When: the pull-request cell combines inference with the stated record.
        let cell = pull_request_cell(&row, true);

        // Then: the number is shown once while the stated state and provenance remain visible.
        assert_eq!(cell, "#106 open (stated)");
    }
    #[test]
    fn different_inferred_and_stated_pull_numbers_are_both_rendered() {
        // Given: an inferred pull request and a distinct stated pull request.
        let mut row = row("feat/alpha", None, Some(pull_request(106)));
        row.stated_pull = Some(StatedPull {
            number: 107,
            state: "OPEN".to_owned(),
        });

        // When: the pull-request cell combines inference with the stated record.
        let cell = pull_request_cell(&row, true);

        // Then: both numbers remain visible because they identify different pull requests.
        assert_eq!(cell, "#106 #107 open (stated)");
    }
    #[test]
    fn a_shadowed_pull_request_renders_as_prior_history_beside_the_primary() {
        // Given: an open pull request whose branch also carries a closed
        // predecessor (an org-fork submission re-homed onto a personal fork).
        let mut row = row("feat/alpha", None, Some(pull_request(4894)));
        row.prior_pulls = vec![PriorPull {
            number: 4565,
            state: "CLOSED".to_owned(),
        }];

        // When: the pull-request cell renders.
        let cell = pull_request_cell(&row, true);

        // Then: the closed predecessor stays visible — its review history is
        // exactly what a reader of the open pull request is missing.
        assert_eq!(cell, "#4894 prior #4565 closed");
    }
    #[test]
    fn a_branch_whose_origin_is_ahead_is_shown_as_behind() {
        // "Is my work pushed" is otherwise unanswerable, and it decides whether
        // a release cut from origin ships the current code.
        let mut row = row("feat/alpha", None, None);
        row.origin_tip = Some(CommitId::new("deadbeefdead"));
        row.origin_relation = Some(OriginRelation::Behind);
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row],
            forge_consulted: true,
            ..Report::default()
        });
        assert!(out.contains("behind"), "was: {out}");
    }
    #[test]
    fn local_ahead_of_origin_is_not_reported_as_behind_it() {
        // One word for both directions was a live bug: unpushed local work and a local copy
        // that is stale read identically, and only one of them invalidates a landed verdict.
        let mut ahead = row("feat/alpha", None, None);
        ahead.tip = Some(CommitId::new("aaaaaaaaaaaa"));
        ahead.origin_tip = Some(CommitId::new("bbbbbbbbbbbb"));
        ahead.origin_relation = Some(OriginRelation::Ahead);
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![ahead],
            ..Report::default()
        };

        let out = render(&report, false);
        assert!(out.contains("unpushed-commits"), "was: {out}");
        assert!(!out.contains("(behind)"), "ahead is not behind: {out}");
    }
    #[test]
    fn diverged_origin_is_not_reported_as_behind() {
        let mut row = row("feat/alpha", None, None);
        row.origin_tip = Some(CommitId::new("deadbeefdead"));
        row.origin_relation = Some(OriginRelation::Diverged);
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row],
            forge_consulted: true,
            ..Report::default()
        });

        assert!(out.contains("(diverged)"), "was: {out}");
        assert!(!out.contains("(behind)"), "was: {out}");
    }
    #[test]
    fn unresolved_origin_relation_is_not_reported_as_history() {
        let mut row = row("feat/alpha", None, None);
        row.origin_tip = Some(CommitId::new("deadbeefdead"));
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row],
            forge_consulted: true,
            ..Report::default()
        });

        assert!(out.contains("(unresolved)"), "was: {out}");
        assert!(!out.contains("(behind)"), "was: {out}");
        assert!(!out.contains("(diverged)"), "was: {out}");
    }
    #[test]
    fn a_branch_with_no_origin_counterpart_is_shown_as_unpushed() {
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row("feat/alpha", None, None)],
            forge_consulted: true,
            ..Report::default()
        });
        assert!(out.contains("unpushed"), "was: {out}");
        assert!(!out.contains("unpushed-commits"), "was: {out}");
    }
    #[test]
    fn a_report_that_did_not_consult_the_forge_says_so() {
        let report = Report {
            repo: "a-repo".to_owned(),
            forge_consulted: false,
            ..Report::default()
        };
        assert!(render(&report, true).contains("not checked"));
    }
}
