//! Whether content is actually carried — by replay and ancestry, never text.
//!
//! The audit's worst near-miss class was a branch deleted while its content
//! was uncarried. The verdicts here are content-based only: sha ancestry for
//! carried-exact, a three-way tree merge for carried-rewritten (jj divergent
//! change-ids force tree comparison — the same change id can name two
//! different trees), and merge conflicts for human judgment. A net-zero
//! revision is vacuously carried: it contributes no content for the target to
//! lack. Every verdict names an evidence commit a notch can cite and a later
//! reader can re-resolve.

use std::collections::BTreeMap;
use std::path::Path;

use crate::detect::{BookmarkTips, RebaseOutcome};
use crate::ids::{
    BookmarkRef, CommitId, ReleaseScheme, is_our_release, strict_dated_release,
};
use crate::jj::Repo;

/// What a revision is checked against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Target {
    /// Every ref naming this commit — `release/X`, `release/X@origin`, … —
    /// so a double-cut shows up as two targets with one name.
    pub refs: Vec<BookmarkRef>,
    pub commit: CommitId,
    pub role: TargetRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetRole {
    /// A ref of the newest release name (or the fixed release branch).
    LiveRelease,
    /// A ref of an older dated name that still exists somewhere ours.
    SupersededRelease,
    /// The upstream trunk.
    UpstreamTrunk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CarryVerdict {
    /// The revision's tip is an ancestor of the target: carried as-is.
    CarriedExact,
    /// Its net tree change leaves the target unchanged, whether the same content
    /// arrived through different commits or the revision itself has no net content.
    CarriedRewritten,
    NotCarried,
    /// The replay conflicted while the target itself is clean: some content
    /// is there or unrelated work touched the same files; judge by eye.
    Conflicted,
}

impl CarryVerdict {
    pub const fn carried(self) -> bool {
        matches!(self, Self::CarriedExact | Self::CarriedRewritten)
    }
}

/// One verdict with the commit that proves it: the revision tip for
/// carried-exact (it IS in the target's ancestry), the target commit
/// otherwise (the tree the replay was judged against).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CarryCheck {
    pub verdict: CarryVerdict,
    pub evidence: CommitId,
}
/// One target's carriage verdict, including how it was classified for exit
/// semantics and the commit that establishes the verdict.
#[derive(Debug, serde::Serialize)]
pub struct TargetCheck {
    /// Ref names, `/`-joined for display; a requested revision when no ref names it.
    pub target: String,
    pub commit: CommitId,
    pub role: TargetRole,
    pub verdict: CarryVerdict,
    pub evidence: CommitId,
}

/// The complete answer to a `release carries` query.
#[derive(Debug, serde::Serialize)]
pub struct CarriesReport {
    pub repo: String,
    pub revision: String,
    pub checks: Vec<TargetCheck>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

/// Render the human-readable form of a multi-target carriage report.
pub fn render_carries(report: &CarriesReport) -> String {
    let mut lines = vec![format!("{}: {}", report.repo, report.revision)];
    for check in &report.checks {
        let verdict = match check.verdict {
            CarryVerdict::CarriedExact => "carried-exact",
            CarryVerdict::CarriedRewritten => "carried-rewritten",
            CarryVerdict::NotCarried => "NOT carried",
            CarryVerdict::Conflicted => "conflicted",
        };
        let reason = match check.verdict {
            CarryVerdict::CarriedExact => "tip is an ancestor",
            CarryVerdict::CarriedRewritten => "no net content remains",
            CarryVerdict::NotCarried => "replay leaves real diffs",
            CarryVerdict::Conflicted => "judge by eye",
        };
        let target = if check.target.is_empty() {
            match check.role {
                TargetRole::UpstreamTrunk => "upstream trunk",
                TargetRole::LiveRelease | TargetRole::SupersededRelease => "unnamed release",
            }
        } else {
            check.target.as_str()
        };
        lines.push(format!(
            "  {verdict:<19}{target} @ {}  (evidence {}: {reason})",
            short(&check.commit),
            short(&check.evidence),
        ));
    }
    lines.extend(report.notes.iter().map(|note| format!("  note: {note}")));
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("  unanswered: {problem}")),
    );
    lines.join("\n")
}

fn short(commit: &CommitId) -> String {
    commit.as_str().chars().take(12).collect()
}

/// Every check target for this repository: each distinct commit named by our
/// release refs (grouped, so one name at two commits is two targets), plus the
/// upstream trunk.
///
/// Live/superseded comes from `strict_dated_release` ordering; under a fixed
/// scheme every release ref of the configured name is Live.
pub fn targets(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    trunk: (&str, CommitId),
) -> Vec<Target> {
    let newest_release = match scheme {
        ReleaseScheme::Dated => tips
            .keys()
            .filter(|reference| is_our_release(reference, scheme))
            .filter_map(|reference| strict_dated_release(reference.branch().as_str()))
            .max(),
        ReleaseScheme::Fixed(_) => None,
    };
    let mut grouped = BTreeMap::<CommitId, (Vec<BookmarkRef>, TargetRole, Option<(String, u32)>)>::new();

    for (reference, commit) in tips
        .iter()
        .filter(|(reference, _)| is_our_release(reference, scheme))
    {
        let dated_name = strict_dated_release(reference.branch().as_str());
        let role = match scheme {
            ReleaseScheme::Dated
                if matches!(
                    (dated_name.as_ref(), newest_release.as_ref()),
                    (Some(dated_name), Some(newest_release)) if dated_name == newest_release
                ) =>
            {
                TargetRole::LiveRelease
            }
            ReleaseScheme::Dated => TargetRole::SupersededRelease,
            ReleaseScheme::Fixed(_) => TargetRole::LiveRelease,
        };
        let entry = grouped.entry(commit.clone()).or_insert_with(|| {
            (
                Vec::new(),
                TargetRole::SupersededRelease,
                dated_name.clone(),
            )
        });
        entry.0.push(reference.clone());
        if role == TargetRole::LiveRelease {
            entry.1 = TargetRole::LiveRelease;
        }
        if dated_name > entry.2 {
            entry.2 = dated_name;
        }
    }

    let mut live = Vec::new();
    let mut superseded = Vec::new();
    for (commit, (refs, role, newest_name)) in grouped {
        let target = Target { refs, commit, role };
        if role == TargetRole::LiveRelease {
            live.push(target);
        } else {
            superseded.push((newest_name, target));
        }
    }
    superseded.sort_by(|(left_name, left_target), (right_name, right_target)| {
        right_name
            .cmp(left_name)
            .then_with(|| left_target.commit.cmp(&right_target.commit))
    });

    let trunk_refs = tips
        .iter()
        .filter_map(|(reference, commit)| {
            let names_trunk = match reference {
                BookmarkRef::Local(branch) => branch.as_str() == trunk.0,
                BookmarkRef::Remote { branch, remote } => trunk
                    .0
                    .strip_suffix(remote.as_str())
                    .and_then(|prefix| prefix.strip_suffix('@'))
                    == Some(branch.as_str()),
            };
            (names_trunk && commit == &trunk.1).then(|| reference.clone())
        })
        .collect();

    live.push(Target {
        refs: trunk_refs,
        commit: trunk.1,
        role: TargetRole::UpstreamTrunk,
    });
    live.extend(superseded.into_iter().map(|(_, target)| target));
    live
}

#[derive(Debug)]
pub struct CheckInput<'a> {
    pub repo_path: &'a Path,
    pub repo: &'a Repo,
    pub revision: &'a str,
    pub tip: &'a CommitId,
}

/// The three-way verdict of one revision against one target.
pub fn check(input: &CheckInput<'_>, target: &Target) -> anyhow::Result<CarryCheck> {
    if input.repo.is_ancestor(input.tip, &target.commit)? {
        return Ok(CarryCheck {
            verdict: CarryVerdict::CarriedExact,
            evidence: input.tip.clone(),
        });
    }

    let verdict = match input.repo.tree_replay_outcome(input.tip, &target.commit)? {
        RebaseOutcome::Empty => CarryVerdict::CarriedRewritten,
        RebaseOutcome::CleanNonEmpty => CarryVerdict::NotCarried,
        RebaseOutcome::Conflicted => CarryVerdict::Conflicted,
    };
    Ok(CarryCheck {
        verdict,
        evidence: target.commit.clone(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use crate::detect::BookmarkTips;
    use crate::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName};

    use super::{TargetRole, targets};

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
    fn one_release_name_at_two_commits_is_two_live_targets() {
        let release = "release/2026-08-30.1";
        let local_ref = local(release);
        let origin_ref = remote(release, "origin");
        let targets = targets(
            &tips(vec![(local_ref.clone(), "a"), (origin_ref.clone(), "b")]),
            &ReleaseScheme::Dated,
            ("trunk", CommitId::new("trunk")),
        );

        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[0].commit, CommitId::new("a"));
        assert_eq!(targets[0].refs, vec![local_ref]);
        assert_eq!(targets[1].role, TargetRole::LiveRelease);
        assert_eq!(targets[1].commit, CommitId::new("b"));
        assert_eq!(targets[1].refs, vec![origin_ref]);
        assert_eq!(targets[2].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn superseded_names_and_the_trunk_are_their_own_roles() {
        let old_local = local("release/2026-08-29");
        let old_origin = remote("release/2026-08-29", "origin");
        let trunk_ref = remote("main", "upstream");
        let targets = targets(
            &tips(vec![
                (old_local.clone(), "old"),
                (old_origin.clone(), "old"),
                (local("release/2026-08-30"), "live"),
                (trunk_ref.clone(), "trunk"),
            ]),
            &ReleaseScheme::Dated,
            ("main@upstream", CommitId::new("trunk")),
        );

        assert_eq!(
            targets.iter().map(|target| target.role).collect::<Vec<_>>(),
            vec![
                TargetRole::LiveRelease,
                TargetRole::UpstreamTrunk,
                TargetRole::SupersededRelease,
            ]
        );
        assert_eq!(targets[0].commit, CommitId::new("live"));
        assert_eq!(targets[1].commit, CommitId::new("trunk"));
        assert_eq!(targets[1].refs, vec![trunk_ref]);
        assert_eq!(targets[2].commit, CommitId::new("old"));
        assert_eq!(targets[2].refs, vec![old_local, old_origin]);
    }

    #[test]
    fn upstream_release_refs_are_not_ours_and_not_targets() {
        let targets = targets(
            &tips(vec![(remote("release/2026-08-30", "upstream"), "upstream")]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].role, TargetRole::UpstreamTrunk);
        assert_eq!(targets[0].commit, CommitId::new("trunk"));
        assert!(targets[0].refs.is_empty());
    }

    #[test]
    fn fixed_release_refs_are_all_live_targets() {
        let targets = targets(
            &tips(vec![
                (local("integration"), "local"),
                (remote("integration", "release"), "release"),
            ]),
            &ReleaseScheme::Fixed(BranchName::new("integration")),
            ("main", CommitId::new("trunk")),
        );

        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[1].role, TargetRole::LiveRelease);
        assert_eq!(targets[2].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn a_commit_named_by_live_and_superseded_releases_is_live() {
        let targets = targets(
            &tips(vec![
                (local("release/2026-08-29"), "shared"),
                (local("release/2026-08-30"), "shared"),
            ]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[0].commit, CommitId::new("shared"));
        assert_eq!(targets[1].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn unparseable_dated_release_prefixes_are_not_live() {
        let targets = targets(
            &tips(vec![(local("release/not-a-date"), "invalid")]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].role, TargetRole::UpstreamTrunk);
        assert_eq!(targets[1].role, TargetRole::SupersededRelease);
    }
}
