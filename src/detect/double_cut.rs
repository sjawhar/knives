//! The same release name existing twice with different content.
//!
//! The incident: one fork's dated release was cut twice in one day — two
//! commits, two trees, one name — and every consumer of the name got
//! whichever copy its remote happened to hold. Present-state detection only:
//! group our release refs by name, and a name whose refs disagree is worth a
//! tree comparison. This module is pure; the caller does the tree compare.

use std::collections::BTreeMap;

use crate::detect::BookmarkTips;
use crate::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, is_our_release};

/// Names whose refs (within the local/origin/release trust boundary) name
/// more than one distinct commit, with every ref of each commit.
pub fn same_name_disagreements(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
) -> Vec<(BranchName, Vec<(BookmarkRef, CommitId)>)> {
    let mut grouped: BTreeMap<BranchName, BTreeMap<CommitId, Vec<BookmarkRef>>> = BTreeMap::new();
    for (reference, commit) in tips {
        if is_our_release(reference, scheme) {
            grouped
                .entry(reference.branch().clone())
                .or_default()
                .entry(commit.clone())
                .or_default()
                .push(reference.clone());
        }
    }
    grouped
        .into_iter()
        .filter_map(|(name, commits)| {
            (commits.len() > 1).then(|| {
                let refs = commits
                    .into_iter()
                    .flat_map(|(commit, mut refs)| {
                        refs.sort_unstable();
                        refs.into_iter()
                            .map(move |reference| (reference, commit.clone()))
                    })
                    .collect();
                (name, refs)
            })
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
    use crate::detect::BookmarkTips;
    use crate::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName};

    fn local(name: &str) -> BookmarkRef {
        BookmarkRef::Local(BranchName::new(name))
    }

    fn remote(name: &str, remote: &str) -> BookmarkRef {
        BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(remote),
        }
    }

    fn tips(entries: Vec<(BookmarkRef, &str)>) -> BookmarkTips {
        entries
            .into_iter()
            .map(|(reference, commit)| (reference, CommitId::new(commit)))
            .collect()
    }

    #[test]
    fn one_release_name_at_two_commits_keeps_every_ref() {
        let name = "release/2026-08-30";
        let local_ref = local(name);
        let origin_ref = remote(name, "origin");
        let found = same_name_disagreements(
            &tips(vec![
                (local_ref.clone(), "aaaaaaaa"),
                (origin_ref.clone(), "bbbbbbbb"),
            ]),
            &ReleaseScheme::Dated,
        );

        assert_eq!(
            found,
            vec![(
                BranchName::new(name),
                vec![
                    (local_ref, CommitId::new("aaaaaaaa")),
                    (origin_ref, CommitId::new("bbbbbbbb")),
                ],
            ),]
        );
    }

    #[test]
    fn agreeing_release_refs_are_not_a_disagreement() {
        assert!(
            same_name_disagreements(
                &tips(vec![
                    (local("release/2026-08-30"), "aaaaaaaa"),
                    (remote("release/2026-08-30", "origin"), "aaaaaaaa"),
                ]),
                &ReleaseScheme::Dated,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_release_ref_on_an_untrusted_remote_never_counts() {
        assert!(
            same_name_disagreements(
                &tips(vec![
                    (local("release/2026-08-30"), "aaaaaaaa"),
                    (remote("release/2026-08-30", "origin"), "aaaaaaaa"),
                    (remote("release/2026-08-30", "upstream"), "bbbbbbbb"),
                ]),
                &ReleaseScheme::Dated,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_fixed_scheme_groups_its_configured_branch() {
        let fixed = "integration";
        let local_ref = local(fixed);
        let release_ref = remote(fixed, "release");
        let found = same_name_disagreements(
            &tips(vec![
                (local_ref.clone(), "aaaaaaaa"),
                (release_ref.clone(), "bbbbbbbb"),
                (local("release/2026-08-30"), "cccccccc"),
                (remote("release/2026-08-30", "origin"), "dddddddd"),
            ]),
            &ReleaseScheme::Fixed(BranchName::new(fixed)),
        );

        assert_eq!(
            found,
            vec![(
                BranchName::new(fixed),
                vec![
                    (local_ref, CommitId::new("aaaaaaaa")),
                    (release_ref, CommitId::new("bbbbbbbb")),
                ],
            ),]
        );
    }
}
