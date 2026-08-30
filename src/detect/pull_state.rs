//! Present-state pull request findings that are free once the facts batch ran.
//!
//! Only the three checks the audit proved matter and cost nothing extra:
//! an open pull request with an empty diff, with a deleted head ref, or with
//! an empty tip commit. Every input here was already fetched; this module adds
//! zero forge traffic, which is what keeps it inside `status` at all.

use crate::detect::{Finding, FindingKind, Subject};
use crate::forge::PullDetails;

/// One open pull request's answered state, as the batch reported it.
#[derive(Debug)]
pub struct PullState<'a> {
    pub number: u64,
    pub open: bool,
    pub details: &'a PullDetails,
}

/// Report each open pull request's answered present-state incidents.
pub fn pull_state_findings(pulls: &[PullState<'_>]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for pull in pulls.iter().filter(|pull| pull.open) {
        if let Some(diff) = pull.details.diff
            && diff.empty()
        {
            findings.push(Finding::new(
                FindingKind::EmptyDiff,
                Subject::PullRequest(pull.number),
                "open pull request changes no files (+0 −0): its content landed elsewhere or was never pushed",
            ));
        }
        if pull.details.head_ref_deleted == Some(true) {
            findings.push(Finding::new(
                FindingKind::DeletedHeadRef,
                Subject::PullRequest(pull.number),
                "open pull request's head ref is gone from the forge",
            ));
        }
        if pull.details.tip_commit_empty == Some(true) {
            findings.push(Finding::new(
                FindingKind::EmptyTipCommit,
                Subject::PullRequest(pull.number),
                "open pull request's tip commit changes nothing",
            ));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{DiffTotals, PullDetails};

    fn details(
        diff: Option<DiffTotals>,
        head_gone: Option<bool>,
        tip_empty: Option<bool>,
    ) -> PullDetails {
        PullDetails {
            diff,
            head_ref_deleted: head_gone,
            tip_commit_empty: tip_empty,
            ..PullDetails::default()
        }
    }

    #[test]
    fn an_open_pull_with_an_answered_empty_diff_is_a_finding() {
        // Given: an open pull request the facts batch answered with zero changes
        let details = details(Some(DiffTotals::default()), Some(false), Some(false));

        // When: present-state incidents are detected
        let findings = pull_state_findings(&[PullState {
            number: 7,
            open: true,
            details: &details,
        }]);

        // Then: its empty diff is reported against that pull request
        let finding = findings.first().expect("the empty diff must be reported");
        assert_eq!(findings.len(), 1);
        assert_eq!(finding.kind, FindingKind::EmptyDiff);
        assert_eq!(finding.subject, Subject::PullRequest(7));
    }

    #[test]
    fn unanswered_fields_and_closed_pulls_produce_nothing() {
        // Given: unanswered facts and a closed pull with every incident signal
        let unanswered = details(None, None, None);
        let closed = details(Some(DiffTotals::default()), Some(true), Some(true));

        // When: present-state incidents are detected
        let findings = pull_state_findings(&[
            PullState {
                number: 7,
                open: true,
                details: &unanswered,
            },
            PullState {
                number: 8,
                open: false,
                details: &closed,
            },
        ]);

        // Then: absent facts are not interpreted as incidents and closed pull state is ignored
        assert!(
            findings.is_empty(),
            "None is not-consulted and closed pulls carry no incident: {findings:?}"
        );
    }

    #[test]
    fn deleted_head_and_empty_tip_are_their_own_findings() {
        // Given: an open pull with content but a deleted head and an empty tip
        let details = details(
            Some(DiffTotals {
                additions: 3,
                deletions: 1,
                changed_files: 1,
            }),
            Some(true),
            Some(true),
        );

        // When: present-state incidents are detected
        let kinds: Vec<_> = pull_state_findings(&[PullState {
            number: 9,
            open: true,
            details: &details,
        }])
        .into_iter()
        .map(|finding| finding.kind)
        .collect();

        // Then: both independent facts are retained in their stable order
        assert_eq!(
            kinds,
            vec![FindingKind::DeletedHeadRef, FindingKind::EmptyTipCommit]
        );
    }

    #[test]
    fn findings_preserve_pull_order_and_order_incidents_within_a_pull() {
        // Given: two open pull requests in reported row order with overlapping incidents
        let first = details(Some(DiffTotals::default()), Some(true), Some(true));
        let second = details(
            Some(DiffTotals {
                additions: 1,
                deletions: 0,
                changed_files: 1,
            }),
            Some(false),
            Some(true),
        );

        // When: present-state incidents are detected
        let findings = pull_state_findings(&[
            PullState {
                number: 12,
                open: true,
                details: &first,
            },
            PullState {
                number: 3,
                open: true,
                details: &second,
            },
        ]);

        // Then: each pull's facts are grouped in report order and have one stable incident order
        let subjects: Vec<_> = findings
            .iter()
            .map(|finding| finding.subject.clone())
            .collect();
        let kinds: Vec<_> = findings.iter().map(|finding| finding.kind).collect();
        assert_eq!(
            subjects,
            vec![
                Subject::PullRequest(12),
                Subject::PullRequest(12),
                Subject::PullRequest(12),
                Subject::PullRequest(3),
            ]
        );
        assert_eq!(
            kinds,
            vec![
                FindingKind::EmptyDiff,
                FindingKind::DeletedHeadRef,
                FindingKind::EmptyTipCommit,
                FindingKind::EmptyTipCommit,
            ]
        );
    }
}
