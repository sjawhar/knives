//! Release parents the branch has moved off.

use std::collections::BTreeMap;

use crate::detect::{Finding, FindingKind, Subject};
use crate::ids::{BookmarkRef, CommitId};

/// One parent of a release merge, with every bookmark pointing at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseParent {
    pub commit: CommitId,
    pub bookmarks: Vec<BookmarkRef>,
}

/// Tip of every bookmark, local and remote kept apart.
pub type BookmarkTips = BTreeMap<BookmarkRef, CommitId>;

/// Flag release parents that nothing points at any more.
///
/// A remote rewrite (a maintainer pressing "update branch", or a force-push)
/// moves the bookmark to a new commit while the merge keeps the old one, so the
/// release silently ships pre-rewrite code. A local rewrite does not do this:
/// jj auto-rebases the merge and carries the bookmark along. That asymmetry is
/// the whole rule, and it is proven by an integration test against real jj
/// rather than assumed.
///
/// A parent is held when any bookmark on it still points at it. Local and remote
/// refs are separate keys, so a remote ref only holds a parent when the remote
/// really still points there.
pub fn stale_parents(parents: &[ReleaseParent], tips: &BookmarkTips) -> Vec<Finding> {
    parents
        .iter()
        .filter(|parent| !is_held(parent, tips))
        .map(|parent| {
            Finding::new(
                FindingKind::StaleParent,
                Subject::Commit(parent.commit.clone()),
                detail(parent, tips),
            )
        })
        .collect()
}

fn is_held(parent: &ReleaseParent, tips: &BookmarkTips) -> bool {
    parent
        .bookmarks
        .iter()
        .any(|reference| tips.get(reference) == Some(&parent.commit))
}

fn detail(parent: &ReleaseParent, tips: &BookmarkTips) -> String {
    let commit = &parent.commit;
    if parent.bookmarks.is_empty() {
        return format!(
            "parent {commit} carries no bookmark, so the release pins a revision \
             nothing points at"
        );
    }
    let moved = parent
        .bookmarks
        .iter()
        .map(|reference| {
            let now = tips
                .get(reference)
                .map_or_else(|| "unknown".to_owned(), ToString::to_string);
            format!("{reference} is now {now}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("parent {commit} is no longer the tip of its branch ({moved})")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ids::{BranchName, RemoteName};

    fn local(name: &str) -> BookmarkRef {
        BookmarkRef::Local(BranchName::new(name))
    }

    fn remote(name: &str, remote: &str) -> BookmarkRef {
        BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(remote),
        }
    }

    fn parent(commit: &str, bookmarks: Vec<BookmarkRef>) -> ReleaseParent {
        ReleaseParent {
            commit: CommitId::new(commit),
            bookmarks,
        }
    }

    fn tips(entries: Vec<(BookmarkRef, &str)>) -> BookmarkTips {
        entries
            .into_iter()
            .map(|(reference, commit)| (reference, CommitId::new(commit)))
            .collect()
    }

    #[test]
    fn no_finding_when_every_parent_holds_its_bookmark() {
        let parents = [parent("0700338c", vec![local("feat/beta")])];
        let tips = tips(vec![(local("feat/beta"), "0700338c")]);
        assert!(stale_parents(&parents, &tips).is_empty());
    }

    #[test]
    fn a_parent_with_no_bookmark_is_stale() {
        // Given: a parent nothing points at
        let parents = [parent("876dc2d6", vec![])];
        let tips = tips(vec![(local("feat/alpha"), "118d0fcf")]);
        // When / Then: reported, saying why
        let findings = stale_parents(&parents, &tips);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].subject.to_string(), "876dc2d6");
        assert!(findings[0].detail.contains("no bookmark"));
    }

    #[test]
    fn a_parent_whose_bookmark_moved_reports_where_it_went() {
        let parents = [parent("876dc2d6", vec![local("feat/alpha")])];
        let tips = tips(vec![(local("feat/alpha"), "118d0fcf")]);
        let findings = stale_parents(&parents, &tips);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("118d0fcf"));
    }

    #[test]
    fn a_remote_ref_holds_a_parent_when_the_remote_still_points_at_it() {
        // A release cut from remote refs is genuinely held by them. Treating
        // remote refs as never-holding reported every such parent as stale.
        let parents = [parent("72193319", vec![remote("release/dated", "origin")])];
        let tips = tips(vec![(remote("release/dated", "origin"), "72193319")]);
        assert!(stale_parents(&parents, &tips).is_empty());
    }

    #[test]
    fn a_local_bookmark_moving_does_not_excuse_a_stale_remote_ref() {
        // Local and remote are separate keys: the local tip matching some other
        // commit must not make a parent look held.
        let parents = [parent("876dc2d6", vec![remote("feat/alpha", "origin")])];
        let tips = tips(vec![
            (local("feat/alpha"), "876dc2d6"),
            (remote("feat/alpha", "origin"), "118d0fcf"),
        ]);
        assert_eq!(stale_parents(&parents, &tips).len(), 1);
    }
}
