//! Whether a branch's content is already upstream.

use std::fmt;

/// What happened when the branch was replayed onto the upstream trunk.
///
/// An enum rather than a pair of booleans on purpose. The previous design took
/// `empty` and `conflicted` as two adjacent flags, which a caller can transpose
/// and no type checker can catch, and which admits a fourth combination that
/// cannot occur. Here the impossible state is simply not representable, and the
/// classifier below needs no error path at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// No diff remains: the content is already there.
    Empty,
    /// The replay conflicted: the content is there but was changed on the way in.
    Conflicted,
    /// A clean, non-empty replay: still carrying real work.
    CleanNonEmpty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LandedVerdict {
    /// Replaying the branch onto the trunk produced nothing, so the trunk already has
    /// this content.
    InTrunk,
    /// Replaying it conflicts with the trunk.
    ///
    /// Deliberately named for what was observed rather than what it might mean. This
    /// used to be called `landed-modified`, asserting that the maintainer had taken the
    /// branch and changed it. A conflict does not say that: a branch declined upstream,
    /// whose files were then touched by unrelated work, conflicts identically. Observed
    /// on a pull request that was closed as declined and reported as upstream with
    /// maintainer changes.
    ConflictsWithTrunk,
    /// Replaying it applies cleanly and is not empty, so the trunk does not have it.
    NotInTrunk,
    /// The local bookmark does not match its origin tip, so the probe would be
    /// replaying content the pull request does not contain.
    ///
    /// This exists because the alternative is worse than saying nothing. The probe
    /// replays whatever the local bookmark points at; when local is behind origin
    /// that is stale content, and stale content replays clean against the trunk and
    /// reads as landed. The advice attached to `landed` is to drop the branch and
    /// its release parent, so a wrong answer here deletes live fork code. Observed
    /// against a real repository, where two branches with open pull requests were reported
    /// landed while nothing of theirs existed upstream.
    Unjudged,
}

impl fmt::Display for LandedVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::InTrunk => "in-trunk",
            Self::ConflictsWithTrunk => "conflicts-with-trunk",
            Self::NotInTrunk => "not-in-trunk",
            // Short: it shares a line with everything else about the branch, and the
            // long form is spelled out once in the unanswered list.
            Self::Unjudged => "landed?",
        };
        f.write_str(text)
    }
}

/// Classify a branch by what replaying it onto the trunk produced.
///
/// Ancestry cannot answer this. A squash merge creates a new commit, so the
/// branch never becomes an ancestor of the trunk even though its content is
/// there; verified in an integration test that asserts exactly that shape.
/// Authorship and pull request numbers are also no help, because our work
/// sometimes lands under someone else's number.
pub const fn classify_landed(outcome: RebaseOutcome) -> LandedVerdict {
    match outcome {
        RebaseOutcome::Empty => LandedVerdict::InTrunk,
        RebaseOutcome::Conflicted => LandedVerdict::ConflictsWithTrunk,
        RebaseOutcome::CleanNonEmpty => LandedVerdict::NotInTrunk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_replay_means_the_trunk_already_has_it() {
        assert_eq!(
            classify_landed(RebaseOutcome::Empty),
            LandedVerdict::InTrunk
        );
    }

    #[test]
    fn a_conflicted_replay_says_only_that_it_conflicts() {
        assert_eq!(
            classify_landed(RebaseOutcome::Conflicted),
            LandedVerdict::ConflictsWithTrunk
        );
    }

    #[test]
    fn a_clean_non_empty_replay_means_the_trunk_lacks_it() {
        assert_eq!(
            classify_landed(RebaseOutcome::CleanNonEmpty),
            LandedVerdict::NotInTrunk
        );
    }

    #[test]
    fn each_verdict_renders_a_distinct_label() {
        let labels = [
            LandedVerdict::InTrunk.to_string(),
            LandedVerdict::ConflictsWithTrunk.to_string(),
            LandedVerdict::NotInTrunk.to_string(),
        ];
        let mut unique = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 3);
    }
}
