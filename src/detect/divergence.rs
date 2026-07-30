//! One change existing as more than one commit.

use std::collections::BTreeMap;

use crate::detect::{Finding, FindingKind, Subject};
use crate::ids::{ChangeId, CommitId};

/// Flag changes that exist as several commits.
///
/// Change ids are identical across disconnected clones, verified by experiment,
/// so the same change rewritten in two places and then fetched produces two
/// commits sharing one id. The general rule: a change rewritten while any other
/// reference still points at its old commit diverges, whether that reference
/// lives in another clone or on a remote.
///
/// Divergence is routine. The observed failure is agents reading the `??`
/// markers and numeric suffixes as corruption and stopping, so the finding
/// carries the cause and the resolution rather than just the fact.
pub fn divergent_changes(commits: &[(ChangeId, CommitId)]) -> Vec<Finding> {
    let mut by_change: BTreeMap<&ChangeId, Vec<&CommitId>> = BTreeMap::new();
    for (change, commit) in commits {
        by_change.entry(change).or_default().push(commit);
    }

    by_change
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(change, mut ids)| {
            ids.sort_unstable();
            let joined = ids
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Finding::new(
                FindingKind::Divergence,
                Subject::Change((*change).clone()),
                format!(
                    "change {change} exists as {} commits ({joined}); it was rewritten while \
                     another reference still pointed at the old commit",
                    ids.len()
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn row(change: &str, commit: &str) -> (ChangeId, CommitId) {
        (ChangeId::new(change), CommitId::new(commit))
    }

    #[test]
    fn no_finding_when_each_change_has_one_commit() {
        let rows = [row("aaaa", "1111"), row("bbbb", "2222")];
        assert!(divergent_changes(&rows).is_empty());
    }

    #[test]
    fn a_change_on_two_commits_names_both() {
        // Given: one change id carried by two commits
        let rows = [
            row("aaaa", "1111"),
            row("aaaa", "2222"),
            row("bbbb", "3333"),
        ];
        // When: the detector runs
        let findings = divergent_changes(&rows);
        // Then: exactly the divergent change is reported, with both commits
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::Divergence);
        assert_eq!(findings[0].subject.to_string(), "aaaa");
        assert!(findings[0].detail.contains("1111"));
        assert!(findings[0].detail.contains("2222"));
    }
}
