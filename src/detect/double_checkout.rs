//! Two workspaces holding `@` on one change.

use std::collections::BTreeMap;

use crate::detect::{Finding, FindingKind, Subject};
use crate::ids::{ChangeId, WorkspaceName};

/// Flag any change that more than one workspace has checked out.
///
/// This is the direct precondition for divergence: whichever workspace runs the
/// next jj command snapshots its own tree into that change, and the other
/// workspace's edits are clobbered or diverged. The check costs one query and
/// would have prevented a real collision.
pub fn double_checkout(workspaces: &[(WorkspaceName, ChangeId)]) -> Vec<Finding> {
    let mut by_change: BTreeMap<&ChangeId, Vec<&WorkspaceName>> = BTreeMap::new();
    for (name, change) in workspaces {
        by_change.entry(change).or_default().push(name);
    }

    by_change
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(change, mut names)| {
            names.sort_unstable();
            let joined = names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Finding::new(
                FindingKind::DoubleCheckout,
                Subject::Change((*change).clone()),
                format!("workspaces {joined} all have @ on change {change}"),
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

    fn row(workspace: &str, change: &str) -> (WorkspaceName, ChangeId) {
        (WorkspaceName::new(workspace), ChangeId::new(change))
    }

    #[test]
    fn no_finding_when_every_workspace_is_on_its_own_change() {
        // Given: two workspaces on distinct changes
        let rows = [row("default", "aaaaaaaa"), row("other", "bbbbbbbb")];
        // When / Then: nothing to report
        assert!(double_checkout(&rows).is_empty());
    }

    #[test]
    fn a_shared_change_names_both_workspaces_and_carries_a_remedy() {
        // Given: two workspaces on one change
        let rows = [row("default", "qxtmtnqn"), row("terminal", "qxtmtnqn")];
        // When: the detector runs
        let findings = double_checkout(&rows);
        // Then: one finding, naming both, with a fix
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::DoubleCheckout);
        assert_eq!(findings[0].subject.to_string(), "qxtmtnqn");
        assert!(findings[0].detail.contains("default"));
        assert!(findings[0].detail.contains("terminal"));
    }

    #[test]
    fn one_finding_per_shared_change_not_one_per_pair() {
        // Given: three workspaces on one change, which is three pairs
        let rows = [row("a", "x"), row("b", "x"), row("c", "x"), row("d", "y")];
        // When: the detector runs
        let findings = double_checkout(&rows);
        // Then: the change is reported once, not three times
        let subjects: Vec<String> = findings.iter().map(|f| f.subject.to_string()).collect();
        assert_eq!(subjects, ["x".to_owned()]);
    }
}
