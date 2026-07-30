//! Work that has been carried somewhere else.
//!
//! The maintainer making their own branch out of your commits looks like nothing at all
//! from the branch's own point of view: it is not merged, not conflicted, and still open.
//! What it is, mechanically, is a tip reachable from some other reference.

use crate::detect::{Finding, FindingKind, Subject};
use crate::ids::{BookmarkRef, BranchName};

/// A branch whose tip is reachable from references other than its own.
///
/// Says where it was found and nothing about what it means: whether the maintainer took
/// the work, rebased it, or coincidentally landed the same content is exactly the judgment
/// this tool leaves to the reader.
pub fn carried_elsewhere(branch: &BranchName, carriers: &[BookmarkRef]) -> Option<Finding> {
    if carriers.is_empty() {
        return None;
    }
    let named: Vec<String> = carriers.iter().map(ToString::to_string).collect();
    Some(Finding::new(
        FindingKind::CarriedElsewhere,
        Subject::Branch(branch.clone()),
        format!("{branch}'s tip is also reachable from {}", named.join(", ")),
    ))
}

#[cfg(test)]
mod tests {
    use super::carried_elsewhere;
    use crate::detect::FindingKind;
    use crate::ids::{BookmarkRef, BranchName, RemoteName};

    #[test]
    fn a_tip_reachable_from_another_reference_is_reported_with_its_carriers() {
        // Given: a branch and an upstream bookmark whose history reaches its tip
        let branch = BranchName::new("feat/alpha");
        let carrier = BookmarkRef::Remote {
            branch: BranchName::new("maintainer/rework"),
            remote: RemoteName::new("upstream"),
        };

        // When: the carrier is classified
        let finding = carried_elsewhere(&branch, &[carrier]).expect("a carrier is a finding");

        // Then: the fact names the carrier without interpreting why it is reachable
        assert_eq!(finding.kind, FindingKind::CarriedElsewhere);
        assert!(
            finding.detail.contains("maintainer/rework@upstream"),
            "{}",
            finding.detail
        );
    }

    #[test]
    fn no_carriers_is_not_a_finding() {
        // Given: a branch with no other reachable bookmark
        let branch = BranchName::new("feat/alpha");

        // When: its empty carrier set is classified
        let finding = carried_elsewhere(&branch, &[]);

        // Then: absence of a carrier creates no finding
        assert!(finding.is_none());
    }
}
