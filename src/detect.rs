//! Pure detectors.
//!
//! Nothing in this module or its children touches a repository, a process, or
//! the network. Every detector is a function from parsed values to findings, so
//! the semantics can be tested exhaustively and cheaply. The parts that do touch
//! a repository live in [`crate::jj`] and hand these functions typed values.

pub mod divergence;
pub mod double_checkout;
pub mod double_cut;
pub mod landed;
pub mod overlap;
pub mod pull_state;
pub mod stale_parents;
pub mod superseded;

use std::fmt;

pub use divergence::divergent_changes;
pub use double_checkout::double_checkout;
pub use landed::{LandedVerdict, RebaseOutcome, classify_landed};
pub use stale_parents::{BookmarkTips, ReleaseParent, stale_parents};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingKind {
    DoubleCheckout,
    DoubleCut,
    StaleParent,
    Divergence,
    StaleReview,
    ClaimOverlap,
    BranchOverlap,
    UnmetDependency,
    Unmergeable,
    WrongBase,
    ChecksFailing,
    CarriedElsewhere,
    SupersededBase,
    EmptyDiff,
    DeletedHeadRef,
    EmptyTipCommit,
    RemoteDrift,
    UnconfiguredRemote,
    ZombieBranch,
    ReleaseDrift,
    OrphanCommit,
    /// A branch whose history beyond the upstream trunk carries merge commits —
    /// typically a release cut — so it carries every member of that cut too.
    StackedHistory,
    /// A claim on a branch that no bookmark or workspace still names.
    OrphanedClaim,
    /// The repository's own jj config states an `immutable_heads()` other than
    /// the one its registry entry runs under (`RepoEntry::immutable_heads`).
    ImmutableHeadsRule,
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::DoubleCheckout => "double-checkout",
            Self::StaleParent => "stale-parent",
            Self::Divergence => "divergence",
            Self::StaleReview => "stale-review",
            Self::UnmetDependency => "unmet-dependency",
            Self::Unmergeable => "unmergeable",
            Self::WrongBase => "wrong-base",
            Self::ChecksFailing => "checks-failing",
            Self::ClaimOverlap => "claim-overlap",
            Self::BranchOverlap => "branch-overlap",
            Self::CarriedElsewhere => "carried-elsewhere",
            Self::SupersededBase => "superseded-base",
            Self::EmptyDiff => "empty-diff",
            Self::DoubleCut => "double-cut",
            Self::DeletedHeadRef => "deleted-head-ref",
            Self::EmptyTipCommit => "empty-tip-commit",
            Self::RemoteDrift => "remote-drift",
            Self::UnconfiguredRemote => "unconfigured-remote",
            Self::ZombieBranch => "zombie-branch",
            Self::ReleaseDrift => "release-drift",
            Self::OrphanCommit => "orphan-commit",
            Self::StackedHistory => "stacked-history",
            Self::OrphanedClaim => "orphaned-claim",
            Self::ImmutableHeadsRule => "immutable-heads-rule",
        };
        f.write_str(text)
    }
}

/// What a finding is about.
///
/// A `String` here erased the distinction between a change id, a commit id and
/// a branch name, and that erasure shipped a bug: a set of change ids was
/// compared against a commit id, so a check silently reported nothing, forever.
/// It also caused branch names to be truncated to twelve characters by a
/// shortener meant for hex ids, producing remedies naming branches that do not
/// exist. Both are compile errors now.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subject {
    Change(crate::ids::ChangeId),
    Commit(crate::ids::CommitId),
    Branch(crate::ids::BranchName),
    Bookmark(crate::ids::BookmarkRef),
    PullRequest(u64),
    File(String),
}

impl Subject {
    /// Short enough to scan. Only identifiers are abbreviated: a truncated
    /// branch name or file path is wrong, not merely terse.
    pub fn short(&self) -> String {
        match self {
            Self::Change(id) => id.short().to_owned(),
            Self::Commit(id) => id.short().to_owned(),
            Self::Branch(name) => name.to_string(),
            Self::Bookmark(reference) => reference.to_string(),
            Self::PullRequest(number) => format!("#{number}"),
            Self::File(path) => path.clone(),
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Change(id) => write!(f, "{id}"),
            Self::Commit(id) => write!(f, "{id}"),
            Self::Branch(name) => write!(f, "{name}"),
            Self::Bookmark(reference) => write!(f, "{reference}"),
            Self::PullRequest(number) => write!(f, "#{number}"),
            Self::File(path) => f.write_str(path),
        }
    }
}

/// Something observed about a repository. Not a recommendation.
///
/// There used to be a `remedy` on every finding. It is gone deliberately: the advice
/// was wrong often enough to be a liability — telling us to drop a branch that had
/// never landed, to open a pull request for one that already existed, to re-cut a
/// release nothing pinned — and being told what to do is not what this tool is for.
/// Report what is, and let the reader decide.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub subject: Subject,
    pub detail: String,
}

impl Finding {
    pub fn new(kind: FindingKind, subject: Subject, detail: impl Into<String>) -> Self {
        Self {
            kind,
            subject,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;

    const fn expected_label(kind: FindingKind) -> &'static str {
        match kind {
            FindingKind::DoubleCheckout => "double-checkout",
            FindingKind::StaleParent => "stale-parent",
            FindingKind::Divergence => "divergence",
            FindingKind::StaleReview => "stale-review",
            FindingKind::ClaimOverlap => "claim-overlap",
            FindingKind::BranchOverlap => "branch-overlap",
            FindingKind::UnmetDependency => "unmet-dependency",
            FindingKind::Unmergeable => "unmergeable",
            FindingKind::WrongBase => "wrong-base",
            FindingKind::ChecksFailing => "checks-failing",
            FindingKind::CarriedElsewhere => "carried-elsewhere",
            FindingKind::SupersededBase => "superseded-base",
            FindingKind::EmptyDiff => "empty-diff",
            FindingKind::DoubleCut => "double-cut",
            FindingKind::DeletedHeadRef => "deleted-head-ref",
            FindingKind::EmptyTipCommit => "empty-tip-commit",
            FindingKind::RemoteDrift => "remote-drift",
            FindingKind::UnconfiguredRemote => "unconfigured-remote",
            FindingKind::ZombieBranch => "zombie-branch",
            FindingKind::ReleaseDrift => "release-drift",
            FindingKind::OrphanCommit => "orphan-commit",
            FindingKind::StackedHistory => "stacked-history",
            FindingKind::OrphanedClaim => "orphaned-claim",
            FindingKind::ImmutableHeadsRule => "immutable-heads-rule",
        }
    }

    #[test]
    fn every_kind_renders_a_stable_label() {
        // Given: the full set of kinds a report can contain
        // Add every new FindingKind here; expected_label cannot make this array exhaustive.
        let kinds = [
            FindingKind::DoubleCheckout,
            FindingKind::StaleParent,
            FindingKind::Divergence,
            FindingKind::StaleReview,
            FindingKind::ClaimOverlap,
            FindingKind::BranchOverlap,
            FindingKind::UnmetDependency,
            FindingKind::Unmergeable,
            FindingKind::WrongBase,
            FindingKind::ChecksFailing,
            FindingKind::CarriedElsewhere,
            FindingKind::SupersededBase,
            FindingKind::EmptyDiff,
            FindingKind::DoubleCut,
            FindingKind::DeletedHeadRef,
            FindingKind::EmptyTipCommit,
            FindingKind::RemoteDrift,
            FindingKind::UnconfiguredRemote,
            FindingKind::ZombieBranch,
            FindingKind::ReleaseDrift,
            FindingKind::OrphanCommit,
            FindingKind::StackedHistory,
            FindingKind::OrphanedClaim,
            FindingKind::ImmutableHeadsRule,
        ];
        // When: each is rendered
        let labels: Vec<String> = kinds
            .into_iter()
            .map(|kind| {
                let expected = expected_label(kind);
                assert_eq!(kind.to_string(), expected);
                expected.to_owned()
            })
            .collect();
        // Then: every label is distinct, so a reader can tell them apart
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
    }
}
