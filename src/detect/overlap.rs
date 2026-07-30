//! Branches that touch the same files.
//!
//! Two branches editing one file conflict when a release merges them, and the cut is a bad
//! time to find out. This is a path comparison and nothing more: whether the edits actually
//! conflict is a question for whoever reads the report.

use std::collections::BTreeMap;

use crate::detect::{Finding, FindingKind, Subject};

/// Files touched by more than one branch, one finding per file.
///
/// One finding per file rather than per pair: three branches on one file is one fact about
/// that file, and three findings saying nearly the same thing is how a report becomes
/// unreadable.
pub fn branch_overlaps(touching: &BTreeMap<String, Vec<String>>) -> Vec<Finding> {
    let mut by_file: BTreeMap<&String, Vec<&String>> = BTreeMap::new();
    for (branch, files) in touching {
        for file in files {
            by_file.entry(file).or_default().push(branch);
        }
    }
    by_file
        .into_iter()
        .filter(|(_, branches)| branches.len() > 1)
        .map(|(file, branches)| {
            let named: Vec<String> = branches.iter().map(ToString::to_string).collect();
            Finding::new(
                FindingKind::BranchOverlap,
                Subject::File(file.clone()),
                format!("{file} is touched by {}", named.join(", ")),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touching(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(branch, files)| {
                (
                    (*branch).to_owned(),
                    files.iter().map(|file| (*file).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn one_file_touched_by_two_branches_is_one_finding_naming_both() {
        // Given: two branches change one shared file and one exclusive file
        let touching = touching(&[
            ("feat/a", &["src/lib.rs", "README.md"]),
            ("feat/b", &["src/lib.rs"]),
        ]);
        // When: overlaps are detected
        let findings = branch_overlaps(&touching);
        // Then: the shared file names both branches exactly once
        assert_eq!(
            findings.len(),
            1,
            "one per file, not per pair: {findings:?}"
        );
        let finding = findings.first().expect("the shared file must be reported");
        assert!(finding.detail.contains("src/lib.rs"));
        assert!(finding.detail.contains("feat/a"));
        assert!(finding.detail.contains("feat/b"));
    }

    #[test]
    fn three_branches_on_one_file_stay_one_finding() {
        // Given: three branches change one file
        let touching = touching(&[
            ("feat/a", &["src/lib.rs"]),
            ("feat/b", &["src/lib.rs"]),
            ("feat/c", &["src/lib.rs"]),
        ]);
        // When: overlaps are detected
        let findings = branch_overlaps(&touching);
        // Then: the file remains one fact, while all branches are named
        assert_eq!(findings.len(), 1);
        let finding = findings.first().expect("the shared file must be reported");
        assert!(finding.detail.contains("feat/a"));
        assert!(finding.detail.contains("feat/b"));
        assert!(finding.detail.contains("feat/c"));
    }

    #[test]
    fn files_touched_by_one_branch_are_not_findings() {
        // Given: distinct files touched by distinct branches
        let touching = touching(&[("feat/a", &["src/lib.rs"]), ("feat/b", &["src/main.rs"])]);
        // When: overlaps are detected
        let findings = branch_overlaps(&touching);
        // Then: neither file is reported
        assert!(findings.is_empty());
    }
}
