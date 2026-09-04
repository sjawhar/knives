//! What is being worked on right now: the claim-overlap finding and the
//! branch-to-workspace naming rule that `status`, `start`, and `finish` share.

use std::collections::BTreeMap;

use crate::detect::{Finding, FindingKind, Subject};

/// Two active claims touching one file.
///
/// File overlap is the strongest duplicate-work signal available, and every
/// real collision observed was same-file. Two agents building the same fix on
/// differently named branches is otherwise undetectable.
pub fn overlaps(touching: &BTreeMap<String, Vec<String>>) -> Vec<Finding> {
    touching
        .iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(file, holders)| {
            Finding::new(
                FindingKind::ClaimOverlap,
                Subject::File(file.clone()),
                format!(
                    "{file} is being changed by {}: {}",
                    holders.len(),
                    holders.join(", ")
                ),
            )
        })
        .collect()
}

/// Which workspace a claim's work lives in.
///
/// `knives start` names a workspace for its branch with slashes flattened, so the
/// mapping is derivable rather than stored.
pub fn workspace_for(branch: &str) -> String {
    branch.replace('/', "-")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn touching(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(file, holders)| {
                (
                    (*file).to_owned(),
                    holders.iter().map(|h| (*h).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn two_claims_on_one_file_are_reported_with_both_holders() {
        let findings = overlaps(&touching(&[("src/x.rs", &["one on r/a", "two on r/b"])]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::ClaimOverlap);
        assert!(findings[0].detail.contains("one on r/a"));
        assert!(findings[0].detail.contains("two on r/b"));
    }

    #[test]
    fn a_file_only_one_claim_touches_is_not_a_finding() {
        assert!(overlaps(&touching(&[("src/x.rs", &["one on r/a"])])).is_empty());
    }

    #[test]
    fn a_workspace_name_is_derived_from_the_branch() {
        // `knives start` flattens slashes, so the mapping needs no storage.
        assert_eq!(workspace_for("feat/alpha"), "feat-alpha");
        assert_eq!(workspace_for("feat/a/b"), "feat-a-b");
    }
}
