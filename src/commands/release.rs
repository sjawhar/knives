//! `knives release`: plan, cut, edit or repair a release.
//!
//! Everything here is a check, never a prompt. A CLI in a non-interactive agent
//! session has nobody to ask, so it decides from evidence and says what it
//! decided. Planning is the default because everything else here writes: a cut
//! names a composition, `include`, `drop`, `advance` and `rebase` change one,
//! and `adopt` records a release finished by hand as the name's newest write.
//! Every write leaves a conflict of only added release-log sections resolved
//! as their union, and refuses while the release in hand carries a change
//! knives did not write or adopt, printing the steps that record it. Every one
//! of them writes locally only, and none of them pushes.
// allow: SIZE_OK: 4044 lines - the release lifecycle's plan, members, cut, edit, adopt refusals and remedies, audit, reap, and rebase operations, and the pull every one of them plans from, are one domain seam.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bind::Fork;
use crate::cli::Exit;
use crate::config::{Registry, RepoEntry};
use crate::consumer_pins::{
    ConsumerHeadMemo, ConsumerPinSource, scan_consumer_for, scan_consumer_slug_with_heads,
};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, RebaseOutcome, ReleaseParent, Subject, stale_parents,
};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, is_our_dated_cut, is_our_release,
    is_release_name,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, Kind, Ledger, RecordedParent};
use crate::pins::{Pin, PinKind};
use crate::release_model::{
    BranchSuccessions, EarlierWrites, MemberEvidence, MemberHistory, MemberSuccession,
    PreviousRelease, RecordedCut, RecordedWrite, ReleaseReading, StackedHistoryContext,
    UnexplainedCommit, carried_from_tips, double_cut_findings, last_recorded_parents,
    matched_write, member_history, member_parents, newest_release, previous_release_label,
    read_release, recorded_release_changes, release_parents, release_refs_by_commit, repo_slug,
    stacked_release, trunk_positions,
};

/// Registered forge consumers and explicitly requested local checkout scans.
pub struct ConsumerInputs<'a> {
    pub slugs: &'a [String],
    pub locals: &'a [PathBuf],
    pub forge: &'a dyn ConsumerPinSource,
    pub cache_root: Option<&'a Path>,
    pub heads: &'a ConsumerHeadMemo,
}

impl std::fmt::Debug for ConsumerInputs<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsumerInputs")
            .field("slugs", &self.slugs)
            .field("locals", &self.locals)
            .field("forge", &"<Forge>")
            .field("cache_root", &self.cache_root)
            .field("heads", self.heads)
            .finish()
    }
}

/// `fork`'s ledger entries, read after pulling them from every destination
/// `registry` gives the fork.
///
/// With them, the pull's problems and each entry or statement the read could
/// not fully read: what a release command decides from, so a write from it
/// refuses on any of them.
pub fn pulled_ledger(
    fork: &Fork<'_>,
    registry: &Registry,
) -> anyhow::Result<(Vec<Entry>, Vec<String>)> {
    let pulled = crate::ledger_sweep::pull(registry, &[&fork.upstream]);
    let read = Ledger::for_fork(fork).read()?;
    let mut problems = pulled.problems_for(&fork.upstream);
    problems.extend(read.problems);
    Ok((read.entries, problems))
}

/// The release plan for `fork`, from a ledger pulled first: what a release
/// command decides from.
///
/// A pull that fails is one of the plan's problems. The plan still answers
/// from the entries this machine has, and a command that would write from it
/// refuses, exactly as it does when a consumer could not be consulted: the
/// drop guard checks a member against the newest recorded cut, and a stale
/// ledger would check it against the wrong one.
pub fn pulled_plan(
    fork: &Fork<'_>,
    registry: &Registry,
    consumers: &ConsumerInputs<'_>,
) -> anyhow::Result<Plan> {
    let (entries, problems) = pulled_ledger(fork, registry)?;
    let mut plan = plan(fork, consumers, &entries)?;
    plan.problems.extend(problems);
    Ok(plan)
}

/// What repairing this release would actually reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairEffect {
    /// At least one consumer follows the branch, so repairing in place reaches
    /// them and a new dated name is not required. A needless dated name burns
    /// the name and forces a re-pin nobody wanted.
    RepairInPlace,
    /// Every pin is frozen, so the next cut has to take a new dated suffix.
    NewDatedName,
    /// Nothing pins it, so either is safe.
    Unpinned,
}

/// What moving or editing `release` would reach, judged by the pins of that
/// release alone.
///
/// A pin is of this release when it names the branch, or when it freezes the
/// exact commit the publish remote holds for it (`published`). The second form
/// is how a lockfile written from an installed commit pins: `?rev=<sha>`, never
/// the release name. Judged by name alone, such a consumer read as no pin at
/// all, so the fork's only release could be edited in place although the edit
/// reached nobody, and a verbatim recut under a new dated name — the one thing
/// that would reach it — was refused as identical.
///
/// A consumer frozen on an older release, or on some other commit, is not
/// reached by an edit to this one either way, and must not block it: judged
/// over every pin, a fork whose consumer sat frozen on one old cut refused to
/// edit any release at all, including a brand-new unpinned cut. A pin names the
/// branch, so the release's local and publish-remote views are the same release
/// to it. Off-scheme pins at a tag or branch of the consumer's own choosing
/// neither receive an in-place repair nor demand a new dated name. Only a
/// frozen pin counts by commit: a follower's lock fragment records where it
/// resolved last time, not where it is held.
pub fn repair_effect(
    pins: &[Pin],
    release: &BranchName,
    published: Option<&CommitId>,
) -> RepairEffect {
    let mut of_release = pins
        .iter()
        .filter(|pin| {
            (pin.on_scheme && pin.reference == release.as_str())
                || (pin.kind == PinKind::Frozen
                    && published.is_some_and(|commit| freezes_commit(pin, commit)))
        })
        .peekable();
    if of_release.peek().is_none() {
        return RepairEffect::Unpinned;
    }
    if of_release.any(|pin| pin.kind == PinKind::Follows) {
        return RepairEffect::RepairInPlace;
    }
    RepairEffect::NewDatedName
}

/// Whether `pin` holds its consumer at exactly `commit`: the reference is the
/// commit itself, or the lock fragment resolved whatever the reference names
/// (`?rev=<tag>#<sha>`) to it. An abbreviation counts from seven hex digits,
/// the shortest git itself prints; six is the lock scanner's floor for a
/// fragment being hex at all, not for it being one commit.
fn freezes_commit(pin: &Pin, commit: &CommitId) -> bool {
    let abbreviates = |candidate: &str| {
        candidate.len() >= 7
            && candidate
                .chars()
                .all(|character| character.is_ascii_hexdigit())
            && commit
                .as_str()
                .get(..candidate.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(candidate))
    };
    abbreviates(&pin.reference) || pin.locked.as_deref().is_some_and(abbreviates)
}

/// A dated release at its local and publish-remote positions, asked whether
/// it is published ([`published_release`]).
#[derive(Debug, Clone, Copy)]
pub struct ReleaseAt<'a> {
    pub scheme: &'a ReleaseScheme,
    pub name: &'a str,
    pub commits: &'a [Option<&'a CommitId>],
}

/// What marks a dated release published, rendered for a human, or `None`.
///
/// A tag names one of its commits, or a `record: published …` note on the
/// release names one in its evidence. A forge release leaves a tag, and the
/// release owner's `record: published …` note says where it was published,
/// whichever reached this machine first. Other `record:` rows (a drop sweep's
/// verdicts on a candidate, say) do not count. An edit moves the release's
/// name, and a push of the moved name would move what consumers were told was
/// published: every edit verb refuses such a release, and a cut under a new
/// dated name may start identical to it. A fixed release branch moves forward
/// by design and leaves its published positions to their tags, so nothing
/// marks it here.
pub fn published_release(
    repo: &Repo,
    ledger: &[Entry],
    release: ReleaseAt<'_>,
) -> Result<Option<String>, crate::jj::JjError> {
    let ReleaseAt {
        scheme,
        name: release,
        commits,
    } = release;
    if matches!(scheme, ReleaseScheme::Fixed(_)) {
        return Ok(None);
    }
    let mut seen: Vec<&CommitId> = Vec::new();
    for commit in commits.iter().flatten() {
        if seen.contains(commit) {
            continue;
        }
        seen.push(commit);
        if let Some(tag) = repo.tags_on(commit)?.first() {
            return Ok(Some(format!("tag {tag} names {}", commit.short())));
        }
    }
    Ok(published_note(ledger, release, &seen))
}

/// The `record: published …` note half of [`published_release`]: the newest
/// such note on `release` that names one of `commits` in its evidence,
/// rendered for a human, or `None`. A note names a commit by its id as a
/// string, so this reads only the ledger.
fn published_note(ledger: &[Entry], release: &str, commits: &[&CommitId]) -> Option<String> {
    let recorded = ledger.iter().rev().find_map(|entry| {
        let is_record = entry.kind == Kind::Note
            && entry.subject.as_deref() == Some(release)
            && entry
                .text
                .trim_start()
                .strip_prefix("record:")
                .is_some_and(|rest| rest.trim_start().starts_with("published"));
        if !is_record {
            return None;
        }
        commits.iter().copied().find(|commit| {
            entry.evidence.iter().any(|evidence| {
                let evidence = evidence.trim();
                evidence.len() >= 7
                    && evidence
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                    && commit
                        .as_str()
                        .get(..evidence.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(evidence))
            })
        })
    });
    recorded.map(|commit| format!("a record: published note names {}", commit.short()))
}

/// Refuse `name` when it is a series' own prefix, such as `release/9.2` on an
/// entry with series 9.2: git cannot hold that ref beside `release/9.2/<date>`.
pub fn clear_of_series_prefixes(entry: &RepoEntry, name: &str) -> Result<(), String> {
    entry
        .series
        .keys()
        .find(|series| name == format!("{}{series}", crate::ids::RELEASE_PREFIX))
        .map_or(Ok(()), |series| {
            Err(format!(
                "{name} is the prefix of series {series}'s cuts ({name}/{}); git cannot \
                 hold both, so name the cut by date",
                crate::ids::CUT_GRAMMAR
            ))
        })
}

/// The name a cut takes.
///
/// A dated name must be one this series owns and one the series' order can
/// read ([`ReleaseScheme::cut_key`]). A name another series owns, such as
/// `release/9.2/<date>` cut from the default series, would be that series'
/// newest cut, carrying this series' members. A name the series owns but the
/// order cannot read, such as `release/9.2/foo`, has no place in the order:
/// it would never be the newest cut, and the reap would never take it.
pub fn cut_name(scheme: &ReleaseScheme, requested: Option<&str>) -> Result<String, String> {
    let example = || {
        scheme
            .cut_shape()
            .replace(crate::ids::CUT_GRAMMAR, "2026-08-03")
    };
    match (scheme, requested) {
        (ReleaseScheme::Dated | ReleaseScheme::Series(_), Some(name))
            if scheme.cut_key(name).is_some() =>
        {
            Ok(name.to_owned())
        }
        (ReleaseScheme::Dated | ReleaseScheme::Series(_), Some(name)) if scheme.owns(name) => {
            Err(format!(
                "{name} is not a name this series can sort; its cuts are named {}, where the \
                 date is followed by nothing, by `.N` or by `-<label>`, e.g. {}",
                scheme.cut_shape(),
                example()
            ))
        }
        (ReleaseScheme::Dated | ReleaseScheme::Series(_), Some(name)) => Err(format!(
            "{name} is not a cut of this series; its cuts are named {}, e.g. {}",
            scheme.cut_shape(),
            example()
        )),
        (ReleaseScheme::Dated | ReleaseScheme::Series(_), None) => Err(format!(
            "a dated release cut needs a name, e.g. {}",
            example()
        )),
        (ReleaseScheme::Fixed(fixed), Some(name)) if name != fixed.as_str() => Err(format!(
            "this repo cuts the fixed release branch {fixed}; drop the name or use {fixed}"
        )),
        (ReleaseScheme::Fixed(fixed), None | Some(_)) => Ok(fixed.to_string()),
    }
}

/// Return the publish remote's previous fixed release position, if it exists.
///
/// This resolves only the remote-tracking reference, never the local bookmark: [`cut`]
/// creates a merge and moves only that local bookmark via `set_bookmark`; it neither pushes
/// nor fetches. It is therefore sound before a push, and is the seam issue #4's pre/post-cut
/// checks will attach to.
pub fn previous_position(repo: &Repo, entry: &RepoEntry) -> Option<(String, CommitId)> {
    let ReleaseScheme::Fixed(fixed) = entry.release_scheme() else {
        return None;
    };
    let remote = entry.publish_remote();
    let reference = format!("{fixed}@{remote}");
    repo.resolve_commit(&reference)
        .ok()
        .map(|commit| (reference, commit))
}

#[cfg(test)]
mod scheme_tests {
    use super::*;
    use crate::ids::{BranchName, ReleaseScheme};

    #[test]
    fn a_dated_cut_requires_a_name_and_a_fixed_cut_supplies_its_own() {
        let dated = ReleaseScheme::Dated;
        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        assert!(cut_name(&dated, None).is_err());
        assert_eq!(
            cut_name(&dated, Some("release/2026-08-03")).unwrap(),
            "release/2026-08-03"
        );
        assert_eq!(cut_name(&fixed, None).unwrap(), "integration");
        assert_eq!(
            cut_name(&fixed, Some("integration")).unwrap(),
            "integration"
        );
        // A stray dated name under the fixed scheme would silently fork the
        // naming; refusing is the only answer that cannot lose a cut.
        assert!(cut_name(&fixed, Some("release/2026-08-03")).is_err());
    }

    #[test]
    fn a_cut_name_must_belong_to_the_series_it_is_cut_from() {
        let dated = ReleaseScheme::Dated;
        let old = ReleaseScheme::Series(crate::ids::SeriesName::new("9.2"));
        // A series cut takes the series' own shape.
        assert_eq!(
            cut_name(&old, Some("release/9.2/2026-10-21")).unwrap(),
            "release/9.2/2026-10-21"
        );
        // Neither series takes the other's name: each would become the other
        // series' newest cut.
        assert!(cut_name(&old, Some("release/2026-10-21")).is_err());
        assert!(cut_name(&dated, Some("release/9.2/2026-10-21")).is_err());
        let error = cut_name(&old, None).unwrap_err();
        assert!(error.contains("release/9.2/2026-08-03"), "{error}");
        // A name the series owns but its order cannot read is refused with
        // the grammar: it would never be the newest cut, nor ever be reaped.
        for name in ["release/9.2/foo", "release/9.2/2026-10-21.1.2"] {
            let error = cut_name(&old, Some(name)).unwrap_err();
            assert!(
                error.contains("release/9.2/YYYY-MM-DD[.N|-<label>]")
                    && error.contains("`-<label>`"),
                "{name}: {error}"
            );
        }
        // A label is a name of the series: the 9.2 line of the prover fork.
        assert_eq!(
            cut_name(&old, Some("release/9.2/2026-11-15-speed-8.0.9")).unwrap(),
            "release/9.2/2026-11-15-speed-8.0.9"
        );
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub repo: String,
    pub base_findings: Vec<Finding>,
    pub release: Option<String>,
    /// The commit the publish remote holds for `release`, when it has one. A
    /// consumer frozen on exactly this commit pins the release without naming
    /// it; [`repair_effect`] needs it to say so. `None` for a cut that has not
    /// been pushed yet, which nothing can have pinned by commit.
    pub published: Option<CommitId>,
    pub parents: Vec<(CommitId, Vec<String>)>,
    /// The parents that are earlier writes of the release, never members
    /// ([`crate::release_model::carriers`]): listed among `parents`,
    /// labelled, and read by no member check.
    pub carriers: Vec<CommitId>,
    /// The previous release a release of a stacked series stacks on
    /// ([`crate::release_model::read_release`]): listed first among
    /// `parents`, labelled, and never a member.
    pub previous: Option<PreviousRelease>,
    pub stale: Vec<Finding>,
    pub pins: Vec<Pin>,
    /// A consumer could not be consulted, so `pins` may lack the pin that
    /// would change the verdict, and no verdict is given.
    pub pins_unknown: bool,
    /// Informational: something worth saying that is not a failure.
    pub notes: Vec<String>,
    /// Could not answer. These, and only these, make the command exit non-zero
    /// for incompleteness. Keying on every note instead would make a routine
    /// remark like "14 superseded releases not scanned" look like a failure.
    pub problems: Vec<String>,
}

impl Plan {
    /// The release's parent list as the plan shows it, and its members, the
    /// parents every member check reads. `None` when the stacked reading
    /// refuses the release: that is one of the plan's problems, and the
    /// list is the release's parents.
    ///
    /// A release of a stacked series that has a previous release
    /// ([`stacked_release`]) lists that release first ([`Plan::previous`]),
    /// then its members and its carriers. Any other release lists its
    /// parents, its carriers among them ([`Plan::read_carriers`]).
    fn read_parents(
        &mut self,
        (repo, entry): (&Repo, &RepoEntry),
        (ledger, tips, trunks): (&[Entry], &BookmarkTips, &[CommitId]),
        (reference, commit): (&BookmarkRef, &CommitId),
    ) -> anyhow::Result<Option<(Vec<ReleaseParent>, Vec<ReleaseParent>)>> {
        let release = reference.branch().as_str();
        match stacked_release(
            repo,
            entry,
            (ledger, tips, trunks),
            (release, self.published.as_ref(), commit),
        )? {
            None => {
                let parents = repo.parents_of(commit.as_str())?;
                let members = self.read_carriers(repo, (release, &parents), ledger)?;
                Ok(Some((parents, members)))
            }
            Some(Ok(reading)) => {
                let mut listed = reading.composition();
                listed.extend(reading.carriers.iter().cloned());
                let members = release_parents(tips, &reading.members);
                self.carriers = reading.carriers;
                self.previous = reading.previous;
                Ok(Some((release_parents(tips, &listed), members)))
            }
            Some(Err(unexplained)) => {
                self.problems.push(format!("{reference}: {unexplained}"));
                self.list_parents(repo.parents_of(commit.as_str())?);
                Ok(None)
            }
        }
    }

    /// Set [`Plan::parents`] from `parents`, each with the bookmarks on it.
    fn list_parents(&mut self, parents: Vec<ReleaseParent>) {
        self.parents = parents
            .into_iter()
            .map(|parent| {
                let names = parent.bookmarks.iter().map(ToString::to_string).collect();
                (parent.commit, names)
            })
            .collect();
    }

    /// Read which of `release`'s `parents` are carriers into
    /// [`Plan::carriers`], from `ledger` and [`Plan::published`]; the
    /// parents without them, which every member check reads.
    ///
    /// A carrier is an earlier write of the release a fast-forward push gave
    /// it, never a member: no member check reads it, and the parent list
    /// labels it.
    fn read_carriers(
        &mut self,
        repo: &Repo,
        (release, parents): (&str, &[ReleaseParent]),
        ledger: &[Entry],
    ) -> anyhow::Result<Vec<ReleaseParent>> {
        let commits: Vec<CommitId> = parents.iter().map(|parent| parent.commit.clone()).collect();
        let (carriers, members) =
            EarlierWrites::read(repo, release, ledger, self.published.as_ref())?
                .partition(&commits);
        self.carriers = carriers;
        Ok(parents
            .iter()
            .filter(|parent| members.contains(&parent.commit))
            .cloned()
            .collect())
    }

    /// The plan's parent list, one line per parent with what holds it, a
    /// carrier of `release` and the previous release of a stacked one
    /// labelled as such. Bookmarks are names read from the repository, so
    /// each is escaped.
    fn parent_lines(&self, release: &str) -> Vec<String> {
        use crate::ledger::inline_human_text;
        let label =
            crate::release_model::carrier_label(BookmarkRef::parse(release).branch().as_str());
        self.parents
            .iter()
            .map(|(commit, names)| {
                let held = if names.is_empty() {
                    "no bookmark".to_owned()
                } else {
                    inline_human_text(&names.join(", "))
                };
                let previous = self
                    .previous
                    .as_ref()
                    .filter(|previous| previous.commit == *commit)
                    .map(|previous| previous_release_label(&previous.name));
                let tag = previous
                    .as_deref()
                    .or_else(|| self.carriers.contains(commit).then_some(label.as_str()));
                tag.map_or_else(
                    || format!("    {}  {held}", commit.short()),
                    |tag| {
                        format!(
                            "    {}  {} ({held})",
                            commit.short(),
                            inline_human_text(tag)
                        )
                    },
                )
            })
            .collect()
    }
}

/// Whether the release in hand already contains the upstream trunk.
///
/// Stated as a fact, not a instruction: a release that does not contain the current trunk
/// is a normal thing to have, and whether to move it is a judgment. It matters when a
/// pull request has merged upstream, because until the release contains the commit that
/// merge landed in, dropping the local branch removes the change from the release too.
/// `knives release rebase` is the operation; this only says where things stand.
///
/// Landed measures against upstream, never our fork's trunk: the local branch can lag or
/// differ from the repository where the pull request was merged.
pub fn trunk_lag(repo: &Repo, release: Option<&str>, upstream_trunk: &str) -> Option<String> {
    let trunk = repo.resolve_commit(upstream_trunk).ok()?;
    let release = release?;
    let commit = repo.resolve_commit(release).ok()?;
    if repo.is_ancestor(&trunk, &commit).unwrap_or(false) {
        return None;
    }
    Some(format!(
        "{release} does not contain the upstream trunk ({})",
        trunk.short()
    ))
}

/// The commit every member of `release` forks from.
///
/// A legacy release carries its base as a trunk-reachable parent; when it also
/// contains older bases, the shared base is the newest — the one every other
/// trunk-reachable parent is an ancestor of (older bases are #11's accumulation
/// damage). A member that lands upstream by merge is itself selected, so the
/// real fork point is then reported as a superseded base.
///
/// A doctrine-flat release names no base among its parents — the base is never
/// a parent — so its fork point is the newest commit every member and the
/// trunk share. `start` bases new branches on it, the plan names superseded
/// bases against it, and the cut audit and `release rebase` measure drift from
/// it; falling back to the trunk tip would charge all upstream drift since the
/// fork to the members and start new branches on the drifted tip.
pub fn shared_base(
    repo: &Repo,
    release: &CommitId,
    trunk_tip: &CommitId,
) -> anyhow::Result<Option<CommitId>> {
    let parents = repo.parent_commits(release.as_str())?;
    let mut bases = Vec::new();
    for parent in &parents {
        if repo.is_ancestor(parent, trunk_tip)? {
            bases.push(parent.clone());
        }
    }

    'candidates: for candidate in &bases {
        for other in &bases {
            if other != candidate && !repo.is_ancestor(other, candidate)? {
                continue 'candidates;
            }
        }
        return Ok(Some(candidate.clone()));
    }
    if !bases.is_empty() {
        // Trunk-reachable parents that do not contain each other: histories
        // criss-cross, and guessing a base here would misattribute content.
        return Ok(None);
    }
    Ok(repo.common_ancestor(&parents, trunk_tip)?)
}

/// What a recut keeps reachable: the commits the orphan gate treats as work.
///
/// Every non-release local bookmark tip; every commit a divergent non-release
/// local bookmark names (it has no single tip, but each target is still
/// bookmarked work — a member whose bookmark also pointed at its merged pull
/// request head read as dropped by the cut that carried it); the previous
/// release's own parents (the cut carries them verbatim by commit id, so they
/// are kept by construction whatever their bookmarks are doing); and the
/// upstream trunk, `trunk`.
pub fn cut_keepers(
    repo: &Repo,
    entry: &RepoEntry,
    tips: &BookmarkTips,
    previous: &CommitId,
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let scheme = &entry.release_scheme();
    let mut keep: Vec<CommitId> = tips
        .iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch) if !is_release_name(branch, scheme) => Some(commit.clone()),
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        })
        .collect();
    for (reference, commits) in repo.conflicted_bookmarks()? {
        if let BookmarkRef::Local(branch) = reference
            && !is_release_name(&branch, scheme)
        {
            keep.extend(commits);
        }
    }
    keep.extend(repo.parent_commits(previous.as_str())?);
    keep.push(repo.resolve_commit(&entry.trunk_ref())?);
    Ok(keep)
}

/// Commits the recut would strand: reachable from the previous release or its
/// local descendants, and from no keeper.
///
/// Keepers are [`cut_keepers`]. The previous cut itself, empty undescribed working copies,
/// and commits identified by our strict dated release bookmarks are excluded as
/// release machinery, not work. A legacy commit that only *describes* itself
/// like release machinery is deliberately reported: refusing it is safer than
/// dropping real work.
#[derive(Debug, Clone, Copy)]
pub struct OrphanedCommitInput<'a> {
    pub repo_path: &'a Path,
    pub previous: &'a CommitId,
    pub keep: &'a [CommitId],
    pub tips: &'a BookmarkTips,
    pub publish_remote: &'a str,
}

pub fn orphaned_commits(
    input: OrphanedCommitInput<'_>,
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let OrphanedCommitInput {
        repo_path,
        previous,
        keep,
        tips,
        publish_remote,
    } = input;
    let keepers = keep
        .iter()
        .map(|commit| commit.as_str().to_owned())
        .collect::<Vec<_>>()
        .join("|");
    let release_commits = our_dated_release_commits(tips, publish_remote);
    let release_commits = if release_commits.is_empty() {
        "none()".to_owned()
    } else {
        release_commits
            .iter()
            .map(CommitId::as_str)
            .collect::<Vec<_>>()
            .join("|")
    };
    let revset = format!(
        "::( {}:: ) ~ ::({}) ~ {} ~ ({release_commits}) ~ (empty() & description(exact:\"\"))",
        previous.as_str(),
        keepers,
        previous.as_str(),
    );
    crate::jj::commits_matching(repo_path, &revset)
}

/// Commit identities currently named by our strict dated release references,
/// of every series: another series' cut is never an orphan of this one.
///
/// This shares the release-name trust boundary used by reap: local, `origin`,
/// and `release` refs count; names merely resembling a release on other
/// remotes do not. Callers subtract these ids rather than trusting descriptions.
pub fn our_dated_release_commits(tips: &BookmarkTips, publish_remote: &str) -> BTreeSet<CommitId> {
    tips.iter()
        .filter(|(reference, _)| is_our_dated_cut(reference, publish_remote))
        .map(|(_, commit)| commit.clone())
        .collect()
}

/// Which refs are ours to reap: everything except `upstream` (somebody
/// else's repository) and `git` (jj's internal tracking view). Applied to
/// the output; the newest-name vote uses [`is_our_release`] so someone else's
/// cut cannot classify the live release as superseded.
fn ours_to_reap(reference: &BookmarkRef) -> bool {
    !matches!(
        reference,
        BookmarkRef::Remote { remote, .. } if matches!(remote.as_str(), "upstream" | "git")
    )
}

/// Every superseded cut of every series of `entry`: what a report that looks
/// at the whole fork, rather than one series, treats as frozen history.
pub fn superseded_cuts_of_every_series(
    tips: &BookmarkTips,
    entry: &RepoEntry,
) -> BTreeSet<BookmarkRef> {
    entry
        .all_series()
        .iter()
        .flat_map(|series| {
            superseded_dated_releases(tips, &series.dated_scheme(), entry.publish_remote())
        })
        .map(|(reference, _)| reference)
        .collect()
}

/// Every ref of every superseded dated cut of `scheme`'s series, on any
/// remote that is ours.
///
/// Superseded means "not the newest dated name of this series voted by a
/// local, `origin`, or `release` ref". The broader output excludes `upstream`
/// and jj's `git` tracking view, but keeps historical refs on every other
/// remote for cleanup. [`ReleaseScheme::cut_key`] keeps upstream-style
/// semver names out even on our remotes, and keeps every other series' cuts
/// out: each series' newest cut is live whatever the others' dates. A name
/// this series owns but its order cannot read has no key, so the reap never
/// takes it.
pub fn superseded_dated_releases(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> Vec<(BookmarkRef, CommitId)> {
    let newest = tips
        .keys()
        // The vote is an allowlist, so someone else's cut cannot reap ours;
        // output remains broader to clean up our historical odd-remote refs.
        .filter(|reference| is_our_release(reference, scheme, publish_remote))
        .filter_map(|reference| scheme.cut_key(reference.branch().as_str()))
        .max();
    let Some(newest) = newest else {
        return Vec::new();
    };
    let mut found: Vec<(BookmarkRef, CommitId)> = tips
        .iter()
        .filter(|(reference, _)| {
            ours_to_reap(reference)
                && scheme
                    .cut_key(reference.branch().as_str())
                    .is_some_and(|parsed| parsed != newest)
        })
        .map(|(reference, commit)| (reference.clone(), commit.clone()))
        .collect();
    // Name-major order keeps refs of one name adjacent; derived `Ord` is
    // variant-major and would interleave names.
    found.sort_by(|(a, _), (b, _)| (a.branch(), a).cmp(&(b.branch(), b)));
    found
}

#[derive(Debug)]
pub struct ReapReport {
    /// Bookmark names whose refs were forgotten AND commits abandoned. A name
    /// whose abandon refused lands in `forgotten_only`, never here (oracle
    /// amendment: reaped must not overstate).
    pub reaped: Vec<String>,
    /// Refs forgotten but the commit kept, with why: still pinned by a ref
    /// outside the enumeration — a tag, an untracked remote bookmark, a local
    /// bookmark of another name — so it is not abandoned. The expected outcome
    /// for a tagged or retained release, not a failure: the name is gone and
    /// the commit stays reachable by its pin. In a stacked series the reap
    /// also puts here each name the live release descends from
    /// ([`forget_superseded_ancestors`]). Nothing else lands here; an abandon
    /// that failed for another reason is a note.
    pub forgotten_only: Vec<(String, String)>,
    /// (name, reason) pairs that were deliberately left alone.
    pub kept: Vec<(String, String)>,
    /// The names among `kept` whose cut is kept only for what that release
    /// published on it ([`Repo::rests_only_on_published`]): kept on purpose,
    /// with nothing for a person to act on.
    pub kept_published: Vec<String>,
    /// Non-fatal notes: an abandon that failed for a reason other than a pin,
    /// and a cleanup of a reaped name's earlier writes that failed.
    pub notes: Vec<String>,
    /// The heads the reap's abandons exposed, hidden in the same operation
    /// ([`crate::jj::HiddenHead`]).
    pub hidden: Vec<crate::jj::HiddenHead>,
}

/// Reap every superseded dated cut: forget its refs everywhere, abandon its commits.
///
/// The newest dated name never appears in the enumeration, and the
/// `previous_position` seam is `Fixed`-scheme-only while dated names are the
/// only thing enumerated, so neither needs a runtime gate here. Two things do:
/// a cut that anything rests on is kept with what rests on it named, by the
/// rule a release edit keeps the commit it replaced by
/// ([`Repo::what_rests_on`]); and while the live cut still carries conflicts,
/// every superseded cut is kept, because the previous cut is the only record of
/// how those conflicts were last resolved — reaping it while the successor is
/// unsettled destroys the record exactly when an abandon-and-recut would need
/// it.
///
/// What rests on a cut is stacked work (#4's third loss mode), or a workspace's
/// working copy on it or above it, empty or not. Abandoning the cut would
/// rebase each of them onto the cut's parents: stacked work can come out
/// conflicted, and a working copy that sits on the cut becomes a fresh merge of the members,
/// conflicted where the cut had resolved them, rewritten from outside its
/// workspace, which can be left stale. The kept name keeps its refs, so a later
/// reap retires it once nothing rests on it: jj drops an empty, undescribed
/// working copy when it moves on, and one with changes stays a descendant until
/// it is rebased off the cut. The commits `ledger` records knives writing under
/// a name count as that name's cut: an edit keeps the commit it replaced for
/// what rests on it, and once no ref names that commit, its name is the only
/// thing that retires it.
///
/// Never touches a remote. A later fetch re-materializes forgotten refs as
/// untracked (jj keeps no memory of forgetting); that is expected, harmless to
/// the default log, and cleared by the next reap. Correctness never depends on
/// reaping having run: the divergence detector ignores these refs regardless.
pub fn reap_superseded(
    repo: &Repo,
    scheme: &ReleaseScheme,
    publish_remote: &str,
    ledger: &[Entry],
) -> anyhow::Result<ReapReport> {
    let repo_path = repo.path();
    let tips = repo.bookmark_tips()?;
    let mut by_name = std::collections::BTreeMap::<String, Vec<CommitId>>::default();
    for (reference, commit) in superseded_dated_releases(&tips, scheme, publish_remote) {
        let targets = by_name.entry(reference.branch().to_string()).or_default();
        if !targets.contains(&commit) {
            targets.push(commit);
        }
    }
    if !by_name.is_empty()
        && let Some((live, commit)) = live_dated_release(&tips, scheme, publish_remote)
        && !crate::jj::conflicted_files(repo_path, commit.as_str())?.is_empty()
    {
        return Ok(ReapReport {
            reaped: Vec::new(),
            forgotten_only: Vec::new(),
            kept: by_name
                .into_keys()
                .map(|name| {
                    (
                        name,
                        format!(
                            "the live cut {live} still carries conflicts; a superseded cut \
                             is the record of their last resolution"
                        ),
                    )
                })
                .collect(),
            kept_published: Vec::new(),
            notes: Vec::new(),
            hidden: Vec::new(),
        });
    }

    let mut report = ReapReport {
        reaped: Vec::new(),
        forgotten_only: Vec::new(),
        kept: Vec::new(),
        kept_published: Vec::new(),
        notes: Vec::new(),
        hidden: Vec::new(),
    };
    let mut entries: Vec<(String, Vec<CommitId>)> = Vec::new();
    let mut reaped_writes: Vec<CommitId> = Vec::new();
    for (name, targets) in by_name {
        // An edit that replaced a commit of this name kept it for what rests
        // on it; once no ref names it, this name is all that will retire it.
        let earlier = earlier_writes(repo, ledger, &name)?;
        let kept = what_keeps_a_name(repo, &name, &targets, &earlier)?;
        let Some(reason) = kept else {
            reaped_writes.extend(earlier);
            entries.push((name, targets));
            continue;
        };
        // Whether the keep is quiet is read over every commit of the name,
        // not only the one that was kept first.
        let mut held = targets;
        for commit in earlier {
            if !held.contains(&commit) {
                held.push(commit);
            }
        }
        if repo.rests_only_on_published(&held, (&name, publish_remote))? {
            report.kept_published.push(name.clone());
        }
        report.kept.push((name, reason));
    }
    if !entries.is_empty() {
        let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
        let operation = format!("knives: reap {}", names.join(", "));
        let live = live_dated_release(&tips, scheme, publish_remote).map(|(_, commit)| commit);
        let outcome = crate::jj::forget_and_abandon(
            repo_path,
            &entries,
            (&reaped_writes, live.as_ref()),
            &operation,
        )?;
        report.reaped = outcome.abandoned;
        report.hidden = outcome.hidden;
        for (name, error) in outcome.refused {
            match error {
                crate::jj::JjError::Immutable { commit, pin } => report
                    .forgotten_only
                    .push((name, format!("{commit} still pinned by {pin}"))),
                crate::jj::JjError::Bookmarked { commit, bookmark } => {
                    report.forgotten_only.push((
                        name,
                        format!("{commit} still pinned by bookmark {bookmark}"),
                    ));
                }
                other => report.notes.push(format!(
                    "{name}: refs forgotten, commit not abandoned: {other}"
                )),
            }
        }
    }
    Ok(report)
}

/// Why superseded name `name` is kept, or `None` when nothing rests on any of
/// its ref commits `targets` or its `earlier` writes: the first of them that
/// something rests on, by [`Repo::what_rests_on`].
fn what_keeps_a_name(
    repo: &Repo,
    name: &str,
    targets: &[CommitId],
    earlier: &[CommitId],
) -> Result<Option<String>, crate::jj::JjError> {
    for target in targets {
        if let Some(reason) = repo.what_rests_on(target)? {
            return Ok(Some(reason));
        }
    }
    for earlier in earlier.iter().filter(|commit| !targets.contains(commit)) {
        if let Some(reason) = repo.what_rests_on(earlier)? {
            return Ok(Some(format!(
                "{}, an earlier {name} commit: {reason}",
                earlier.short()
            )));
        }
    }
    Ok(None)
}

/// Reap each superseded dated cut of a stacked series that the live release
/// descends from, and abandon nothing.
///
/// Its refs are forgotten locally and on tracking remotes, as ONE operation.
/// Each name comes back with why its commit stays: `<live> descends from it`.
///
/// Each release of a stacked series is one merge of the previous release
/// and its new members, so the live release carries every earlier release
/// on its first-parent line. Their commits stay reachable from it, and
/// abandoning one would rewrite the live release itself; only the names go.
/// A name with a ref at a commit the live release does not descend from is
/// left to [`reap_superseded`], whose rules keep it when something rests on
/// it. Like that reap, this touches no remote.
pub fn forget_superseded_ancestors(
    repo: &Repo,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let tips = repo.bookmark_tips()?;
    let Some((live, live_commit)) = live_dated_release(&tips, scheme, publish_remote) else {
        return Ok(Vec::new());
    };
    let mut by_name = std::collections::BTreeMap::<String, Vec<CommitId>>::default();
    for (reference, commit) in superseded_dated_releases(&tips, scheme, publish_remote) {
        by_name
            .entry(reference.branch().to_string())
            .or_default()
            .push(commit);
    }
    let mut names = Vec::new();
    'names: for (name, targets) in by_name {
        for target in &targets {
            if !repo.is_ancestor(target, &live_commit)? {
                continue 'names;
            }
        }
        names.push(name);
    }
    crate::jj::forget_bookmarks(
        repo.path(),
        &names,
        &format!("knives: reap {}", names.join(", ")),
    )?;
    Ok(names
        .into_iter()
        .map(|name| (name, format!("{live} descends from it")))
        .collect())
}

/// The commits the ledger records knives writing under `name` that this
/// repository holds, with every visible rewrite of each.
///
/// A `jj rebase` or `jj describe` that moved a kept write (and the working copy
/// on it) left the recorded id hidden and the commit that matters under a new
/// one. A write this repository does not hold (another machine's) is left out;
/// any other failure is an error.
pub fn earlier_writes(
    repo: &Repo,
    ledger: &[Entry],
    name: &str,
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let mut writes = Vec::new();
    for recorded in crate::release_model::recorded_writes(ledger, name).map(|write| write.commit) {
        if repo.resolve_commit(recorded.as_str()).is_err() {
            continue;
        }
        for commit in std::iter::once(recorded.clone()).chain(repo.rewrites_of(recorded.as_str())?)
        {
            if !writes.contains(&commit) {
                writes.push(commit);
            }
        }
    }
    Ok(writes)
}

/// The release in hand, as [`in_hand_write`] reads it against the writes the
/// ledger records under its name.
#[derive(Debug, Clone, Copy)]
pub struct InHand<'a> {
    pub repo: &'a Repo,
    pub ledger: &'a [Entry],
    pub name: &'a str,
    /// The release's commit: its local bookmark's, or the remote ref's where
    /// it has none.
    pub commit: &'a CommitId,
    /// Every earlier write of the name ([`EarlierWrites`]), read with
    /// `published`.
    pub writes: &'a EarlierWrites,
    /// The publish remote's commit of the name, as the caller reads that ref.
    pub published: Option<&'a CommitId>,
}

/// How a release's commit in hand stands to the writes the ledger records
/// under its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InHandWrite {
    /// It holds no change of its own that knives did not write or adopt: it
    /// is a recorded write, or one re-signed or metaedited
    /// ([`Repo::is_metadata_rewrite`]), or its tree is the matched write
    /// rebased onto its members ([`Repo::holds_rebased_write`]).
    Recorded,
    /// It carries a change no knives write or adopt recorded: its tree is not
    /// `write`, the matched write ([`matched_write`]), rebased onto its
    /// members. Every write refuses it until `adopt` records it.
    Unrecorded { write: RecordedWrite },
    /// Nothing here can measure the release against the writes the ledger
    /// records of its name ([`Unchecked`]). A write over it proceeds and says
    /// so, except over a release whose every parent is an earlier write
    /// ([`NoMatch::AllCarriers`], whether or not the ledger records a write of
    /// the name), which every write and `adopt` refuse.
    Unchecked(Unchecked),
    /// The ledger records no write of the name: a release older than the
    /// ledger, or one made with no knives at all. No write refuses it, and
    /// nothing measured it, so a write over it proceeds, says so, and marks
    /// its event unchecked ([`crate::release_model::UNCHECKED_NO_RECORD`]),
    /// which every later write over it inherits ([`Unchecked::Inherited`])
    /// until `adopt` records it. `adopt` has no write of the release itself
    /// to replace, so a cut's carried rows say the ledger records no write of
    /// it and that a person must check each divergence by hand
    /// ([`CarriedFrom::Unledgered`]). A release whose every parent is an
    /// earlier write is [`Self::Unchecked`] instead.
    NoRecord,
}

/// Why a release in hand could not be checked against its recorded writes
/// ([`InHandWrite::Unchecked`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unchecked {
    /// The recorded write its members match is not in this repository: one
    /// made on another machine and never fetched, or pushed only as a copy.
    WriteAbsent(RecordedWrite),
    /// Its members match no recorded write: they changed outside knives.
    NoMatch(NoMatch),
    /// It is the recorded write it equals or matches, or holds that write's
    /// tree, and that write's own event says knives could not check the
    /// release it was written over (`reason`). Nothing measured that release
    /// since, so it stays unchecked until `adopt` records it.
    Inherited {
        write: RecordedWrite,
        reason: String,
    },
}

impl Unchecked {
    /// The mark the ledger event of a write over such a release ends with
    /// (`; unchecked: <mark>`), after every part `recorded_writes` parses: an
    /// inherited one keeps the reason it was first marked with, which is one
    /// of these two or the mark of a write over a release the ledger records
    /// no write of ([`unchecked_suffix`], [`crate::release_model::unchecked_mark`]).
    pub fn mark(&self) -> &str {
        match self {
            Self::WriteAbsent(_) => crate::release_model::UNCHECKED_WRITE_ABSENT,
            Self::NoMatch(_) => crate::release_model::UNCHECKED_NO_MATCH,
            Self::Inherited { reason, .. } => reason,
        }
    }
}

/// The shape of a release whose members match no recorded write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoMatch {
    /// Every parent of the release is an earlier write of it: it is a commit
    /// made on top of the release, whose change the release commit should
    /// hold. No write can compose from it: it has no member. `recorded` says
    /// the ledger records a write of the name for `adopt` to replace.
    AllCarriers { into: CommitId, recorded: bool },
    /// Its members differ from every recorded write's.
    Members,
}

/// The ledger event text's suffix for a write over the release in hand that
/// `standing` describes.
///
/// It is `; unchecked: <mark>` for an unchecked one ([`Unchecked::mark`])
/// and for one the ledger records no write of
/// ([`crate::release_model::UNCHECKED_NO_RECORD`]), nothing otherwise.
pub fn unchecked_suffix(standing: &InHandWrite) -> String {
    let mark = match standing {
        InHandWrite::Unchecked(unchecked) => unchecked.mark(),
        InHandWrite::NoRecord => crate::release_model::UNCHECKED_NO_RECORD,
        InHandWrite::Recorded | InHandWrite::Unrecorded { .. } => return String::new(),
    };
    format!("{}{mark}", crate::release_model::UNCHECKED_DELIMITER)
}

/// Whether `release`'s commit carries a change knives did not write or adopt.
///
/// A divergence the release already has is a recorded resolution only when
/// it does not: a hand edit nothing recorded may have dropped a member's
/// change, and a write built from it would carry the loss forward under the
/// name, after which it would read as knives' own. So every write over such a
/// release refuses until `knives release adopt` measures and records it.
///
/// Only the tree decides, and only against a recorded write this repository
/// holds. The commit id alone decides nothing: a member rewritten in place
/// makes jj rebase the release, a fast-forward push gives it a carrier, and
/// `jj metaedit` re-signs it, none of which is a hand edit. Where the matched
/// write is not here, or no recorded write has the release's members, there
/// is nothing to build the tree from, so the release is unchecked and a write
/// proceeds as it always has, saying so. The members a write is matched by
/// are the release's reading in `entry`'s series ([`release_reading`]).
pub fn in_hand_write(
    release: &InHand<'_>,
    entry: &RepoEntry,
) -> Result<InHandWrite, crate::jj::JjError> {
    let InHand {
        repo,
        ledger,
        name,
        commit,
        writes,
        ..
    } = *release;
    let mut any = false;
    for (event, write) in crate::release_model::recorded_write_events(ledger, name) {
        any = true;
        if repo.is_same_or_metadata_rewrite(&write.commit, commit)? {
            return Ok(unless_marked(event, write, InHandWrite::Recorded));
        }
    }
    // A release whose every parent is a carrier has no member to compose
    // from, so the in-between commit a write builds never stands on no
    // parent, whether or not the ledger records a write of the name. The
    // parents are the commit's own: a stacked release that only starts on
    // its previous release has that release as its one parent, no carrier.
    let parents = repo.parent_commits(commit.as_str())?;
    if let Some(into) = parents.first()
        && writes.carriers(&parents).len() == parents.len()
    {
        return Ok(InHandWrite::Unchecked(Unchecked::NoMatch(
            NoMatch::AllCarriers {
                into: into.clone(),
                recorded: any,
            },
        )));
    }
    if !any {
        return Ok(InHandWrite::NoRecord);
    }
    // The members a recorded write is matched by are the release's reading:
    // in a stacked series the side parents on its first-parent line, never
    // its previous release. A release the reading refuses has no members to
    // match.
    let Ok(reading) = release_reading(repo, entry, ledger, (writes, commit))? else {
        return Ok(InHandWrite::Unchecked(Unchecked::NoMatch(NoMatch::Members)));
    };
    let Some(matched) = matched_write(repo, (ledger, name), writes, &reading.members)? else {
        return Ok(InHandWrite::Unchecked(Unchecked::NoMatch(NoMatch::Members)));
    };
    // A write made on another machine and never fetched is not held here,
    // so the tree it would have here cannot be built.
    if repo.resolve_commit(matched.write.commit.as_str()).is_err() {
        return Ok(InHandWrite::Unchecked(Unchecked::WriteAbsent(
            matched.write,
        )));
    }
    // A write marked unchecked leaves the release unchecked, whatever its
    // tree holds ([`Unchecked::Inherited`]).
    if let Some(reason) = crate::release_model::unchecked_mark(matched.event) {
        return Ok(InHandWrite::Unchecked(Unchecked::Inherited {
            write: matched.write,
            reason: reason.to_owned(),
        }));
    }
    let write_carriers =
        writes.recorded_carriers(repo, &repo.parent_commits(matched.write.commit.as_str())?)?;
    // A stacked release holds its previous release whole, so the write is
    // rebased onto that release and the members, in parent order.
    Ok(
        if repo.holds_rebased_write(
            (&matched.write.commit, &write_carriers),
            &reading.composition(),
            commit,
        )? {
            InHandWrite::Recorded
        } else {
            InHandWrite::Unrecorded {
                write: matched.write,
            }
        },
    )
}

/// How `commit`, a release whose earlier writes are `writes`, reads
/// ([`read_release`]).
///
/// The bookmark tips and the trunk positions the stacked reading walks with
/// are read only for a stacked series; any other release reads from its own
/// parents.
pub fn release_reading(
    repo: &Repo,
    entry: &RepoEntry,
    ledger: &[Entry],
    (writes, commit): (&EarlierWrites, &CommitId),
) -> Result<Result<ReleaseReading, UnexplainedCommit>, crate::jj::JjError> {
    let (tips, trunks) = if entry.stacked() {
        (repo.bookmark_tips()?, trunk_positions(repo, entry)?)
    } else {
        (BookmarkTips::default(), Vec::new())
    };
    read_release(repo, entry, (ledger, &tips, &trunks), (writes, commit))
}

/// How a release in hand measured against `write` stands (`held`), unless
/// `event`, the write's own, says knives could not check the release it was
/// written over: then the release is unchecked too ([`Unchecked::Inherited`]),
/// a hand edit on top included, since nothing measured what it stands on.
fn unless_marked(event: &Entry, write: RecordedWrite, held: InHandWrite) -> InHandWrite {
    crate::release_model::unchecked_mark(event).map_or(held, |reason| {
        InHandWrite::Unchecked(Unchecked::Inherited {
            write,
            reason: reason.to_owned(),
        })
    })
}

/// Whether the publish remote holds the release at `commit`.
///
/// It does when its ref of the name is the commit or descends from it. A
/// pushed hand edit is one consumers were given, so it is `adopt`'s to
/// record, and the published remedy never sends a person to a commit the
/// publish remote already holds.
pub fn publish_remote_holds(
    repo: &Repo,
    commit: &CommitId,
    published: Option<&CommitId>,
) -> Result<bool, crate::jj::JjError> {
    match published {
        Some(published) => Ok(published == commit || repo.is_ancestor(commit, published)?),
        None => Ok(false),
    }
}

/// `the recorded edit 0123456789ab`.
pub fn described_write(write: &RecordedWrite) -> String {
    format!("the recorded {} {}", write.kind, write.commit.short())
}

/// The remedy for a release whose members match no recorded write: a write
/// that changes the composition, which records it.
const NO_MATCH_REMEDY: &str = "make the change with `knives release include`, `drop` or \
                               `advance` (a member added by hand: drop it, then include it); \
                               that write records the new composition, marked unchecked";

/// The refusal of a release whose members match no recorded write.
///
/// It is one line after the repository's name ([`NoMatch`]): `adopt` refuses
/// such a release, and every write does when its every parent is an earlier
/// write.
///
/// That shape is a resolution made as a commit on top of the release, for
/// the release commit to hold; the remedy follows the state. A published
/// release takes the published remedy ([`published_remedy_text`]), from the
/// write the resolution sits on. Otherwise the resolution is squashed into
/// the release commit, with `--ignore-immutable` when that commit is on the
/// publish remote, then adopted, and for a published fixed release adopted
/// again after the fast-forward rebase; a name with no recorded write has
/// nothing to adopt against, so the write is run again.
pub fn no_match_refusal(
    release: &InHand<'_>,
    no_match: &NoMatch,
    entry: &RepoEntry,
) -> Result<String, crate::jj::JjError> {
    let InHand {
        repo,
        ledger,
        name,
        commit,
        published,
        ..
    } = *release;
    let scheme = &entry.release_scheme();
    let publish_remote = entry.publish_remote();
    // The release is a name read from the repository.
    let shown = crate::ledger::inline_human_text(name);
    let NoMatch::AllCarriers { into, recorded } = no_match else {
        return Ok(format!(
            "{shown}'s members differ from every recorded write; {NO_MATCH_REMEDY}"
        ));
    };
    if let Some(evidence) = published_release(
        repo,
        ledger,
        ReleaseAt {
            scheme,
            name,
            commits: &[Some(commit), published, Some(into)],
        },
    )? {
        return published_remedy_text(
            repo,
            (name, commit, &evidence),
            (entry, published),
            Some(into),
        );
    }
    let on_remote = publish_remote_holds(repo, into, published)?;
    let squash = if on_remote {
        format!(
            "`jj squash --from {} --into {} --use-destination-message --ignore-immutable`, as \
             that commit is published",
            commit.short(),
            into.short()
        )
    } else {
        format!(
            "`jj squash --from {} --into {} --use-destination-message`",
            commit.short(),
            into.short()
        )
    };
    let then = match (
        *recorded,
        on_remote && matches!(scheme, ReleaseScheme::Fixed(_)),
    ) {
        (false, _) => "then run the write again",
        (true, true) => "then adopt, and adopt again after the fast-forward rebase",
        (true, false) => "then adopt",
    };
    // A name held only as the publish remote's ref (a fresh clone of a
    // release another pod pushed) has no local bookmark for the squash to
    // move or for adopt to read, so it is tracked first.
    let track = if repo.local_bookmark_tip(name).is_none() {
        format!("track it (`jj bookmark track {shown}@{publish_remote}`), then ")
    } else {
        String::new()
    };
    Ok(format!(
        "every parent of {shown} is an earlier write: {track}squash the resolution into the \
         release commit ({squash}), {then}"
    ))
}

/// The remedy for a release whose name the ledger records no write of:
/// `adopt` has nothing to replace until a write records the name, so it
/// records the name a write over the release writes, whose event that write
/// marks. It states a rule, true whether or not the verb then writes.
const NO_RECORD_REMEDY: &str =
    "a write over it is marked unchecked until `knives release adopt` records the name it writes";

/// The one line a write over a release in hand knives could not check
/// prints before it writes ([`Unchecked`], [`InHandWrite::NoRecord`]).
///
/// It says knives could not check it, why, that knives did not fetch, and
/// what records it. `None` for a release knives checked, and for an
/// unrecorded one, which every write refuses before it gets here.
pub fn unchecked_line(name: &str, commit: &CommitId, standing: &InHandWrite) -> Option<String> {
    // `None` is a name the ledger records no write of.
    let unchecked = match standing {
        InHandWrite::Unchecked(unchecked) => Some(unchecked),
        InHandWrite::NoRecord => None,
        InHandWrite::Recorded | InHandWrite::Unrecorded { .. } => return None,
    };
    // The release is a name read from the repository.
    let shown = crate::ledger::inline_human_text(name);
    let at = commit.short();
    let adopt = format!("`knives release adopt {shown}` records it");
    let (why, remedy) = match unchecked {
        Some(Unchecked::WriteAbsent(write)) => (
            format!(
                "the recorded write its members match, {}, is not in this repository",
                described_write(write)
            ),
            adopt,
        ),
        Some(Unchecked::NoMatch(_)) => (
            "its members match no recorded write, so they changed outside knives".to_owned(),
            NO_MATCH_REMEDY.to_owned(),
        ),
        Some(Unchecked::Inherited { write, reason }) => (
            format!(
                "the release in hand is unchecked since {} ({reason})",
                described_write(write)
            ),
            adopt,
        ),
        None => (
            "the ledger records no write of the name".to_owned(),
            NO_RECORD_REMEDY.to_owned(),
        ),
    };
    Some(format!(
        "knives could not check {shown} at {at} against its recorded writes: {why}; knives did \
         not fetch; {remedy}"
    ))
}

/// The one remedy for a hand edit of a published release.
///
/// It is one line after the repository's name: `adopt` and every write print
/// it. Consumers were given the published commit, so the edit `commit` (R)
/// cannot be recorded under its name; it is redone under a new dated name.
/// The name moves to its published commit, or for a resolution made as a
/// commit on top of the release to the recorded write it sits on (`on`), with
/// `--allow-backwards` where the local name sits at R. The working copy moves
/// off R to the trunk, never onto the write, where the new cut's restore
/// would land in it. Where R is a descendant of the move's target that the
/// publish remote lacks, the cut leaves R behind, so it takes `--allow-drop`
/// and must find R and what rests on it still visible. Where the publish
/// remote names another commit than the target, the cut reads the name as
/// a double cut once. R is redone on the new name, and adopted; R itself,
/// where the remote lacks it, is abandoned when nothing else rests on it,
/// else the remedy names what does and stops. Last, the old name is
/// forgotten here, never deleted, which a push would carry to the remote.
pub fn published_remedy_text(
    repo: &Repo,
    (name, commit, evidence): (&str, &CommitId, &str),
    (entry, published): (&RepoEntry, Option<&CommitId>),
    on: Option<&CommitId>,
) -> Result<String, crate::jj::JjError> {
    // The release is a name read from the repository; the evidence quotes a
    // tag or a ledger entry.
    let publish_remote = entry.publish_remote();
    let shown = crate::ledger::inline_human_text(name);
    let evidence = crate::ledger::inline_human_text(evidence);
    let target = on.or(published);
    let backwards = match target {
        Some(target) if repo.local_bookmark_tip(name).as_ref() == Some(commit) => format!(
            " (`jj bookmark set {shown} -r {} --allow-backwards`)",
            target.short()
        ),
        _ => String::new(),
    };
    let mut steps = vec![match (on, target) {
        (Some(on), _) => format!(
            "move {shown} to {}, the recorded write the resolution sits on{backwards}",
            on.short()
        ),
        (None, Some(published)) => format!(
            "move {shown} back to its published commit ({shown}@{publish_remote}, \
             {}){backwards}",
            published.short()
        ),
        (None, None) => format!("move {shown} back to its published commit"),
    }];
    steps.push(format!(
        "if the working copy is on {} or on an empty commit on it, `jj new 'trunk()'`",
        commit.short()
    ));
    let remote_holds = publish_remote_holds(repo, commit, published)?;
    let mut cut = vec!["cut a new dated name".to_owned()];
    // What the cut's orphan gate will find once the name is moved: R and
    // what rests on it, unless a bookmark keeps them.
    let dropped = match target {
        Some(target) => {
            let mut tips = repo.bookmark_tips()?;
            tips.insert(BookmarkRef::Local(BranchName::new(name)), target.clone());
            let keep = cut_keepers(repo, entry, &tips, target)?;
            orphaned_commits(OrphanedCommitInput {
                repo_path: repo.path(),
                previous: target,
                keep: &keep,
                tips: &tips,
                publish_remote,
            })?
        }
        None => Vec::new(),
    };
    if !dropped.is_empty() {
        // R first, then what rests on it, newest first as the gate lists them.
        let mut named: Vec<&str> = dropped.iter().map(CommitId::short).collect();
        named.sort_by_key(|short| *short != commit.short());
        cut.push(format!(
            "with `--allow-drop`, which must print `--allow-drop: nothing dropped` naming \
             exactly {} (any other answer is a stop)",
            named.join(", ")
        ));
    }
    if published.is_some_and(|published| Some(published) != target) {
        cut.push(format!(
            "(that cut exits 1 once, on `{shown} names both`, and the steps after it end that)"
        ));
    }
    steps.push(cut.join(" "));
    steps.push(format!(
        "`jj restore --from {} --into <new name>`, then adopt the new name",
        commit.short()
    ));
    if !remote_holds {
        if let Some(kept) = repo.what_else_keeps(commit, name)? {
            return Ok(format!(
                "{shown} is published ({evidence}), so a hand edit cannot be recorded under it: \
                 {}; then stop: {} is not to be abandoned, as {kept}",
                steps.join(", then "),
                commit.short()
            ));
        }
        steps.push(format!("`jj abandon {}`", commit.short()));
    }
    steps.push(format!(
        "`jj bookmark forget {shown}` (never `jj bookmark delete`, which a push would carry to \
         the remote)"
    ));
    Ok(format!(
        "{shown} is published ({evidence}), so a hand edit cannot be recorded under it: {}",
        steps.join(", then ")
    ))
}

/// The published remedy for a hand edit of `name` at `commit`
/// ([`published_remedy_text`]), as one line after the repository's name.
///
/// `None` when the publish remote holds the commit ([`publish_remote_holds`])
/// or nothing marks the release published ([`published_release`], read over
/// the commit and `published`, the publish remote's commit of the name).
pub fn published_hand_edit_refusal(
    (repo, ledger): (&Repo, &[Entry]),
    (name, commit, published): (&str, &CommitId, Option<&CommitId>),
    entry: &RepoEntry,
) -> Result<Option<String>, crate::jj::JjError> {
    if publish_remote_holds(repo, commit, published)? {
        return Ok(None);
    }
    let Some(evidence) = published_release(
        repo,
        ledger,
        ReleaseAt {
            scheme: &entry.release_scheme(),
            name,
            commits: &[Some(commit), published],
        },
    )?
    else {
        return Ok(None);
    };
    Ok(Some(published_remedy_text(
        repo,
        (name, commit, &evidence),
        (entry, published),
        None,
    )?))
}

/// Why a write over the release in hand refuses, as one line after the
/// repository's name, or `None` when it does not.
///
/// Only an unrecorded release refuses ([`InHandWrite::Unrecorded`]): it is
/// recorded with `knives release adopt`, unless it is a published release
/// the publish remote does not hold ([`published_hand_edit_refusal`]):
/// `adopt` refuses that too, so the write prints the published remedy
/// instead. A release whose every parent is a carrier refuses too, with the
/// remedy `adopt` gives it ([`no_match_refusal`]): a write has no member to
/// compose it from.
pub fn in_hand_refusal(
    release: &InHand<'_>,
    standing: &InHandWrite,
    entry: &RepoEntry,
) -> Result<Option<String>, crate::jj::JjError> {
    let InHand {
        repo,
        ledger,
        name,
        commit,
        published,
        ..
    } = *release;
    let write = match standing {
        InHandWrite::Unrecorded { write } => write,
        InHandWrite::Unchecked(Unchecked::NoMatch(no_match @ NoMatch::AllCarriers { .. })) => {
            return Ok(Some(no_match_refusal(release, no_match, entry)?));
        }
        InHandWrite::Recorded | InHandWrite::Unchecked(_) | InHandWrite::NoRecord => {
            return Ok(None);
        }
    };
    if let Some(remedy) =
        published_hand_edit_refusal((repo, ledger), (name, commit, published), entry)?
    {
        return Ok(Some(remedy));
    }
    let shown = crate::ledger::inline_human_text(name);
    Ok(Some(format!(
        "{shown} at {} carries a change no knives write or adopt recorded: its tree is not {} \
         rebased onto its members; record it with `knives release adopt {shown}` (adopt asks for \
         --why only for a change outside the conflicts)",
        commit.short(),
        described_write(write)
    )))
}

/// How the release a cut is held against stands, for its carried rows
/// ([`carried_row`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarriedFrom {
    /// Knives' own write ([`InHandWrite::Recorded`]).
    Recorded,
    /// One knives could not check against its recorded writes
    /// ([`InHandWrite::Unchecked`]).
    Unchecked,
    /// One the ledger records no write of ([`InHandWrite::NoRecord`]), or no
    /// release at all. Nothing measured where it diverges from a member, and
    /// `adopt` refuses a release with no recorded write, so its row says a
    /// person must check that divergence by hand.
    Unledgered,
}

/// The row for a member the audited release diverges from exactly where the
/// release it is held against already did ([`CutAudit::carried`]).
///
/// Held against a recorded write, the divergence is a resolution knives
/// wrote or `adopt` recorded, and the row says it is carried forward. Held
/// against an unchecked release, nothing measured it, and the row says to
/// record the release with `adopt` and cut again. Held against a release the
/// ledger records no write of, nothing measured it either, and `adopt`
/// refuses such a release, so the row says the ledger records no write of it
/// and to check the divergence by hand. A release finished by hand and never
/// adopted refuses the write before any row is printed.
pub fn carried_row(name: &str, from: CarriedFrom) -> String {
    // Each name is a member's bookmark, read from the repository.
    let name = crate::ledger::inline_human_text(name);
    match from {
        CarriedFrom::Recorded => format!(
            "  {name}: diverges where the release in hand (a recorded write) already does; \
             carried forward"
        ),
        CarriedFrom::Unchecked => format!(
            "  {name}: diverges where the unrecorded in-hand release already does; record it \
             with `knives release adopt` and cut again"
        ),
        CarriedFrom::Unledgered => format!(
            "  {name}: the ledger records no write of the release in hand, so nothing \
             measured its divergence from this member; check that divergence by hand"
        ),
    }
}

/// The newest dated cut's branch name — unqualified, unlike
/// [`previous_release_for_cut`]'s — and its live commit: the local ref when
/// present, otherwise whichever of our remotes carries it.
///
/// One selection, one order: [`newest_release`] reads the same key. A fixed
/// release has no dated cut, so it has none of these.
pub fn live_dated_release(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> Option<(BranchName, CommitId)> {
    match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => {
            newest_release(tips, scheme, publish_remote)
                .map(|(reference, commit)| (reference.branch().clone(), commit))
        }
        ReleaseScheme::Fixed(_) => None,
    }
}

/// Each member's history measured against the others: a merge whose side
/// lines another member carries is that member's content, not this one's, and
/// is noted; a merge carrying a line no other member has is a finding.
fn member_histories(
    context: StackedHistoryContext<'_>,
    parents: &[ReleaseParent],
    member_parents: &[ReleaseParent],
    plan: &mut Plan,
) -> anyhow::Result<()> {
    for parent in member_parents {
        let others: Vec<CommitId> = parents
            .iter()
            .filter(|other| other.commit != parent.commit)
            .map(|other| other.commit.clone())
            .collect();
        let label = parent_label(parent);
        match member_history(
            StackedHistoryContext {
                carried: &others,
                ..context
            },
            &label,
            &parent.commit,
        )? {
            MemberHistory::Linear => {}
            MemberHistory::CarriedMerges { merges, .. } => plan.notes.push(format!(
                "member {label} carries merge(s) {} whose side lines the other members already \
                 carry",
                merges
                    .iter()
                    .map(CommitId::short)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            MemberHistory::Stacked(finding) => plan.base_findings.push(finding),
        }
    }
    Ok(())
}

/// The plan's note for a series with no cut yet.
fn no_cut_yet(scheme: &ReleaseScheme) -> String {
    match scheme {
        ReleaseScheme::Dated => {
            "no dated release found; the first cut has nothing to repair".to_owned()
        }
        ReleaseScheme::Series(series) => {
            format!("series {series} has no cut yet; the first cut has nothing to repair")
        }
        ReleaseScheme::Fixed(fixed) => {
            format!(
                "fixed release branch {fixed} has no cut yet; the first cut has nothing to repair"
            )
        }
    }
}

/// The members of the release `commit` that the trunk tip does not reach:
/// the ones every member check reads. A member it reaches is a base, and
/// one other than the release's shared base is a superseded base, a finding
/// of `plan`.
fn unlanded_members(
    repo: &Repo,
    (commit, trunk_tip): (&CommitId, Option<&CommitId>),
    members: &[ReleaseParent],
    plan: &mut Plan,
) -> anyhow::Result<Vec<ReleaseParent>> {
    let base = match trunk_tip {
        Some(trunk) => shared_base(repo, commit, trunk)?,
        None => None,
    };
    let mut member_parents = Vec::new();
    for parent in members {
        let trunk_reachable = match trunk_tip {
            Some(trunk) => repo.is_ancestor(&parent.commit, trunk)?,
            None => false,
        };
        if !trunk_reachable {
            member_parents.push(parent.clone());
        } else if base
            .as_ref()
            .is_some_and(|candidate| candidate != &parent.commit)
        {
            plan.base_findings.push(Finding::new(
                FindingKind::SupersededBase,
                Subject::Commit(parent.commit.clone()),
                format!(
                    "parent {} is an older upstream base superseded by {}; \
                     `knives release rebase` self-heals this",
                    parent.commit.short(),
                    base.as_ref().map_or("", CommitId::short),
                ),
            ));
        }
    }
    Ok(member_parents)
}

/// The release in hand and what it does not carry.
///
/// `ledger` is this repository's ledger, read by the caller: the last recorded
/// parent set is what names a member whose branch was rebased outside jj or
/// landed upstream, where the repository no longer can.
pub fn plan(
    fork: &Fork<'_>,
    consumers: &ConsumerInputs<'_>,
    ledger: &[Entry],
) -> anyhow::Result<Plan> {
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let mut plan = Plan {
        repo: fork.name.to_string(),
        ..Plan::default()
    };
    let repo = Repo::open(path)?;
    let tips = repo.bookmark_tips()?;

    // The newest release we cut. Historical ones are frozen and not our concern.
    let scheme = entry.release_scheme();
    let (findings, notes) = double_cut_findings(path, &tips, &scheme, entry.publish_remote())?;
    plan.base_findings.extend(findings);
    plan.notes.extend(notes);
    let publish_remote = entry.publish_remote();
    let newest = newest_release(&tips, &scheme, publish_remote);

    let Some((reference, commit)) = newest else {
        plan.notes.push(no_cut_yet(&scheme));
        return Ok(plan);
    };
    plan.release = Some(reference.to_string());
    plan.published = tips
        .get(&BookmarkRef::Remote {
            branch: reference.branch().clone(),
            remote: RemoteName::new(publish_remote),
        })
        .cloned();

    let trunks = trunk_positions(&repo, entry)?;
    let Some((parents, members)) = plan.read_parents(
        (&repo, entry),
        (ledger, &tips, &trunks),
        (&reference, &commit),
    )?
    else {
        add_consumer_pins(&mut plan, fork, consumers, &scheme);
        return Ok(plan);
    };
    let trunk_tip = repo.resolve_commit(&entry.trunk_ref()).ok();
    let member_parents =
        unlanded_members(&repo, (&commit, trunk_tip.as_ref()), &members, &mut plan)?;
    let releases = release_refs_by_commit(&tips, &scheme, publish_remote);
    let in_hand = [commit.clone()];
    let written = recorded_release_changes(&repo, ledger, &scheme);
    // A member of a stacked release may stack on its previous release, so
    // its history is measured past that release as past the trunk. Branch
    // succession and the trunk's own messages still read the trunk alone.
    let measured: std::borrow::Cow<'_, [CommitId]> =
        plan.previous
            .as_ref()
            .map_or(std::borrow::Cow::Borrowed(&trunks), |previous| {
                std::borrow::Cow::Owned(
                    trunks
                        .iter()
                        .chain(std::iter::once(&previous.commit))
                        .cloned()
                        .collect(),
                )
            });
    let stacked_context = (!trunks.is_empty()).then_some(StackedHistoryContext {
        repo: &repo,
        trunks: &measured,
        releases: &releases,
        carried: &in_hand,
        written: &written,
        replaced: None,
    });
    if let Some(context) = stacked_context {
        member_histories(context, &members, &member_parents, &mut plan)?;
    }
    plan.stale = stale_parents(&member_parents, &tips);
    // Only the selected series' branches are candidates; one that fits no
    // series is named with the reason, since every verb would refuse it.
    let (series_branches, unfit) = crate::release_model::series_members(
        &repo,
        entry,
        carried_from_tips(&tips, entry.trunk(), &scheme),
    )?;
    plan.notes.extend(unfit);
    plan.notes.extend(local_branch_notes(&LocalBranchNotes {
        repo: &repo,
        reference: &reference.to_string(),
        release: &commit,
        parents: &members,
        trunks: &trunks,
        stacked: stacked_context,
        stacked_series: entry.stacked(),
        branches: &series_branches,
        recorded: last_recorded_parents(ledger, &reference.to_string()),
    })?);
    plan.list_parents(parents);

    add_consumer_pins(&mut plan, fork, consumers, &scheme);
    Ok(plan)
}

fn add_consumer_pins(
    plan: &mut Plan,
    fork: &Fork<'_>,
    consumers: &ConsumerInputs<'_>,
    scheme: &ReleaseScheme,
) {
    if consumers.slugs.is_empty() && consumers.locals.is_empty() {
        // An answer, not a gap: `consumers` is optional, and a fork consumed by an
        // install rather than a lockfile has no consumer to record and no path to
        // pass. Read as a problem, it refused every edit and rebase of such a fork
        // while the same plan said nothing pinned it. The render's verdict line
        // draws the conclusion; this note only says what was not consulted.
        plan.notes.push(
            "no consumers recorded; if a lockfile pins this fork, add `consumers = [...]` to \
             the registry entry or pass --consumer"
                .to_owned(),
        );
    }
    // Every consumer, not one: they can sit on different releases, so a plan that saw only
    // the first would call a release unpinned while something else was frozen on it.
    let slug = repo_slug(fork.entry);
    let consulted = plan.problems.len();
    for consumer in consumers.slugs {
        let scan = scan_consumer_slug_with_heads(
            consumers.forge,
            consumers.cache_root,
            &fork.checkout.path,
            consumer,
            slug.as_deref(),
            scheme,
            consumers.heads,
        );
        plan.pins.extend(scan.pins);
        plan.notes.extend(scan.notes);
        plan.problems.extend(scan.problems);
    }
    for consumer in consumers.locals {
        let scan = scan_consumer_for(consumer, slug.as_deref(), scheme);
        plan.pins.extend(scan.pins);
        plan.notes.extend(scan.notes);
        // The scan speaks of one checkout; the plan may hold several, so the
        // problem names which.
        plan.problems.extend(
            scan.problems
                .into_iter()
                .map(|problem| format!("{}: {problem}", consumer.display())),
        );
    }
    plan.pins_unknown = plan.problems.len() > consulted;
}

/// What the plan says about local branches the release does not carry.
struct LocalBranchNotes<'a> {
    repo: &'a Repo,
    reference: &'a str,
    release: &'a CommitId,
    /// Every parent, a landed member's included: the record may still name a
    /// branch at a parent the trunk has since reached.
    parents: &'a [ReleaseParent],
    trunks: &'a [CommitId],
    stacked: Option<StackedHistoryContext<'a>>,
    /// Whether the series is stacked ([`RepoEntry::stacked`]): its next cut
    /// starts on the release in hand, so a branch built on that release is
    /// that cut's to include.
    stacked_series: bool,
    branches: &'a [(String, CommitId)],
    /// The release's last recorded parent set, for a branch the repository can
    /// no longer tie to its parent.
    recorded: &'a [RecordedParent],
}

/// The plan's note for `branch`, at `tip`, built on top of the release in
/// hand.
///
/// Outside a stacked series it is neither a member nor includable as it
/// stands: carrying it would put the cut in its own successor's ancestry. In
/// a stacked series the release in hand is the previous release of the next
/// cut, and such a branch is what that cut includes. So its history is
/// measured as `include` measures it once the next release is cut: past the
/// trunk, the release's own previous release and the release itself. A
/// branch linear past them, or whose merges join only lines the release
/// carries, is fit for the next release; one that carries more is refused
/// as `include` refuses it.
fn built_on_release_note(
    input: &LocalBranchNotes<'_>,
    branch: &str,
    tip: &CommitId,
) -> anyhow::Result<String> {
    let reference = input.reference;
    let Some(context) = input.stacked.filter(|_| input.stacked_series) else {
        return Ok(format!(
            "{branch} is stacked on {reference} rather than the trunk; rebase it off the \
             trunk before including it"
        ));
    };
    let bases: Vec<CommitId> = context
        .trunks
        .iter()
        .chain(std::iter::once(input.release))
        .cloned()
        .collect();
    let measured = StackedHistoryContext {
        trunks: &bases,
        ..context
    };
    Ok(match member_history(measured, branch, tip)? {
        MemberHistory::Stacked(stacked) => format!(
            "{}; rebase it off the trunk before including it",
            stacked.detail
        ),
        MemberHistory::Linear | MemberHistory::CarriedMerges { .. } => format!(
            "{branch} is built on {reference}, which the next cut of this stacked series starts \
             on; `knives release cut <name>` makes that cut, and `knives release include \
             {branch}` then adds it"
        ),
    })
}

/// Whether `branch`'s history carries merges the release in hand already
/// carries (`Ok(true)`), none (`Ok(false)`), or more than its own line: `Err`
/// with the note to print, as `include` measures it.
fn branch_history_note(
    stacked: Option<StackedHistoryContext<'_>>,
    branch: &str,
    tip: &CommitId,
) -> anyhow::Result<Result<bool, String>> {
    let Some(context) = stacked else {
        return Ok(Ok(false));
    };
    Ok(match member_history(context, branch, tip)? {
        MemberHistory::Stacked(stacked) => Err(format!(
            "{}; rebase it off the trunk before including it",
            stacked.detail
        )),
        MemberHistory::CarriedMerges { .. } => Ok(true),
        MemberHistory::Linear => Ok(false),
    })
}

/// For a branch whose history carries merges the release already carries:
/// whether it builds on the members it descends from (`Ok(true)`, `include`
/// adds it beside them, the rule the edit applies), continues one member
/// (`Ok(false)`), or, continuing one, carries that member's line too (`Err`
/// with the note): measured, as `advance` measures it, against the members it
/// would not replace.
fn merge_branch_verdict(
    stacked: Option<StackedHistoryContext<'_>>,
    parents: &[CommitId],
    recorded: &[RecordedParent],
    (lookup, branch, tip): (&crate::release_model::MemberLookup, &str, &CommitId),
) -> anyhow::Result<Result<bool, String>> {
    let Some(context) = stacked else {
        return Ok(Ok(false));
    };
    let members = StackedHistoryContext {
        carried: parents,
        ..context
    };
    if crate::release_model::integrates(members, recorded, lookup, (branch, tip))? {
        return Ok(Ok(true));
    }
    let Some(replaced) = crate::release_model::replaced_member(recorded, lookup, branch) else {
        return Ok(Ok(false));
    };
    Ok(
        match crate::release_model::carries_member_line(members, replaced, (branch, tip))? {
            MemberHistory::Stacked(stacked) => Err(format!(
                "{}; rebase it off the trunk before advancing onto it",
                stacked.detail
            )),
            MemberHistory::CarriedMerges { .. } | MemberHistory::Linear => Ok(false),
        },
    )
}

/// One note per local branch a cut will not carry: membership is the release's
/// parent set, and a branch joins or moves only through a stated `include` or
/// `advance`. Saying so is what keeps "it exists locally" from silently
/// meaning "it ships", without anyone having to remember to ask. Each note
/// names the verb that would actually take the branch, so it never points at
/// an `include` that verb would refuse. A branch the trunk already has, tip
/// included, is said so whether or not the release carries a parent of it:
/// `include` and `advance` both refuse a tip the trunk reaches
/// ([`trunk_reaches`]), and the note that read one as merely absent sent the
/// reader to `include` for a fix the trunk had, when a rebase was the way in.
fn local_branch_notes(input: &LocalBranchNotes<'_>) -> anyhow::Result<Vec<String>> {
    let LocalBranchNotes {
        repo,
        reference,
        release,
        parents,
        trunks,
        stacked,
        stacked_series: _,
        branches,
        recorded,
    } = *input;
    let parents: Vec<CommitId> = parents.iter().map(|parent| parent.commit.clone()).collect();
    let mut notes = Vec::new();
    for (branch, tip) in branches {
        if repo.is_ancestor(tip, release)? {
            continue;
        }
        // A branch built on top of the release descends from every member, which
        // ancestry alone reads as "advanced". It is never advanced: carrying it
        // would put the cut in its own successor's ancestry. Whether it is
        // includable depends on the series ([`built_on_release_note`]).
        if repo.is_ancestor(release, tip)? {
            notes.push(built_on_release_note(input, branch, tip)?);
            continue;
        }
        let merged = match branch_history_note(stacked, branch, tip)? {
            Ok(merged) => merged,
            Err(note) => {
                notes.push(note);
                continue;
            }
        };
        let succession = MemberSuccession::of(repo, trunks, tip)?;
        let lookup = member_parents(&succession, &parents, recorded, branch)?;
        let integrates = if merged {
            match merge_branch_verdict(stacked, &parents, recorded, (&lookup, branch, tip))? {
                Ok(integrates) => integrates,
                Err(note) => {
                    notes.push(note);
                    continue;
                }
            }
        } else {
            false
        };
        if succession.tip_landed()? {
            notes.push(lookup.parents.first().map_or_else(
                || {
                    format!(
                        "{branch} is not in {reference}: the trunk already has it, merged after \
                         the release's base; `knives release rebase` onto a trunk that has it \
                         brings it in"
                    )
                },
                |parent| {
                    format!(
                        "{branch} was released as {} in {reference} and the trunk now has the \
                         whole branch, tip included; `knives release rebase` retires the landed \
                         parent and brings the rest in",
                        parent.short()
                    )
                },
            ));
            continue;
        }
        notes.push(match (lookup.parents.first(), lookup.evidence) {
            (None, _) => {
                format!("{branch} is not in {reference}; `knives release include {branch}` adds it")
            }
            (Some(_), MemberEvidence::Succession) if integrates => format!(
                "{branch} merges {} member(s) of {reference} and builds on them; `knives \
                 release include {branch}` adds it beside them",
                lookup.parents.len()
            ),
            (Some(_), MemberEvidence::Succession) => format!(
                "{branch} has advanced past its parent in {reference}; \
                 `knives release advance {branch}` moves it"
            ),
            (Some(parent), MemberEvidence::Record) => format!(
                "{branch} was released as {} in {reference} and has since been rebased outside \
                 jj; `knives release advance {branch}` moves it through that record",
                parent.short()
            ),
            (Some(parent), MemberEvidence::LandedRecord) => format!(
                "{branch}'s released parent {} in {reference} has landed upstream and the \
                 branch kept going; `knives release advance {branch}` moves it, \
                 `knives release rebase` retires the landed parent",
                parent.short()
            ),
        });
    }
    Ok(notes)
}

/// The name a stacked-history finding uses for one release parent: its
/// local bookmark when one still holds it, otherwise the bare commit — a
/// parent that has been renamed or lost its bookmark is still a real member.
fn parent_label(parent: &ReleaseParent) -> String {
    parent
        .bookmarks
        .iter()
        .find_map(|reference| match reference {
            BookmarkRef::Local(branch) => Some(branch.to_string()),
            BookmarkRef::Remote { .. } => None,
        })
        .unwrap_or_else(|| parent.commit.short().to_owned())
}

/// One direct parent of a release, and what still identifies its commit.
///
/// In a stacked series, a row is one commit of the release's composition:
/// its previous release, a member or a carrier
/// ([`crate::release_model::read_release`]).
#[derive(Debug, serde::Serialize)]
pub struct MemberRow {
    pub commit: CommitId,
    /// Every bookmark still on the parent, verbatim — a `keep/…` anchor is a
    /// bookmark like any other and shows up by name.
    pub held_by: Vec<String>,
    /// Branches that continue the parent ([`BranchSuccessions`]), rendered as
    /// `feat/x advanced to <tip12>`. Empty + empty `held_by` = a bare commit.
    pub advanced: Vec<String>,
    /// A trunk-reachable parent is the base of a legacy cut, not a member.
    pub base_parent: bool,
    /// An earlier write of the release a fast-forward push gave it
    /// ([`crate::release_model::carriers`]): no member, and labelled so.
    pub carrier: bool,
    /// The name of the previous release a stacked release stacks on, when
    /// the row is that release: no member, and labelled so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_release: Option<String>,
}

/// The structural and semantic membership of one release.
#[derive(Debug, serde::Serialize)]
pub struct MembersReport {
    pub repo: String,
    pub release: String,
    pub commit: CommitId,
    /// The repository's own parent list — `git rev-list --parents` semantics
    /// via jj-lib, never text (the audit's worst instrument error was counting
    /// `^parent` lines in commit-message prose).
    pub parent_count: usize,
    pub members: Vec<MemberRow>,
    /// `--verify` only: `audit_cut`'s four buckets, reusing the cut path's phrasing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<CutAudit>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

/// Gather one parent's current holders, descendants, and trunk relation. A
/// carrier is no branch's parent, so no branch is read as continuing it.
fn member_row(
    opened: &Repo,
    (parent, carrier): (ReleaseParent, bool),
    trunk: Option<&CommitId>,
    successions: &BranchSuccessions<'_>,
) -> anyhow::Result<MemberRow> {
    let advanced = if carrier {
        Vec::new()
    } else {
        successions
            .successors_of(&parent.commit)?
            .into_iter()
            .map(|(branch, tip)| format!("{branch} advanced to {}", tip.short()))
            .collect()
    };
    let base_parent = trunk.map_or(Ok(false), |trunk| opened.is_ancestor(&parent.commit, trunk))?;
    Ok(MemberRow {
        commit: parent.commit,
        held_by: parent
            .bookmarks
            .into_iter()
            .map(|bookmark| bookmark.to_string())
            .collect(),
        advanced,
        base_parent,
        carrier,
        previous_release: None,
    })
}

/// The rows of a stacked release that has a previous release: that release
/// on a row of its own, then each member, then each carrier. No branch is
/// read as continuing the previous release, which every member descends
/// from.
fn stacked_rows(
    opened: &Repo,
    reading: &ReleaseReading,
    (tips, trunk): (&BookmarkTips, Option<&CommitId>),
    successions: &BranchSuccessions<'_>,
) -> anyhow::Result<Vec<MemberRow>> {
    let mut rows = Vec::new();
    if let Some(previous) = &reading.previous {
        rows.push(MemberRow {
            commit: previous.commit.clone(),
            held_by: tips
                .iter()
                .filter(|(_, tip)| **tip == previous.commit)
                .map(|(bookmark, _)| bookmark.to_string())
                .collect(),
            advanced: Vec::new(),
            base_parent: false,
            carrier: false,
            previous_release: Some(previous.name.clone()),
        });
    }
    for (commits, carrier) in [(&reading.members, false), (&reading.carriers, true)] {
        for parent in release_parents(tips, commits) {
            rows.push(member_row(opened, (parent, carrier), trunk, successions)?);
        }
    }
    Ok(rows)
}

/// The rows of a release's direct parents, each carrier among them labelled
/// from `ledger` and `published`, the publish remote's commit of `release`.
fn parent_rows(
    opened: &Repo,
    parents: Vec<ReleaseParent>,
    (release, ledger, published): (&str, &[Entry], Option<&CommitId>),
    (trunk, successions): (Option<&CommitId>, &BranchSuccessions<'_>),
) -> anyhow::Result<Vec<MemberRow>> {
    let parent_commits: Vec<CommitId> =
        parents.iter().map(|parent| parent.commit.clone()).collect();
    let carriers =
        crate::release_model::carriers(opened, release, &parent_commits, (ledger, published))?;
    parents
        .into_iter()
        .map(|parent| {
            let carrier = carriers.contains(&parent.commit);
            member_row(opened, (parent, carrier), trunk, successions)
        })
        .collect()
}

/// Whether a row is one the content audit holds the release to: neither a
/// legacy cut's base, an earlier write of the release, nor the previous
/// release of a stacked one.
const fn is_member(member: &MemberRow) -> bool {
    !member.base_parent && !member.carrier && member.previous_release.is_none()
}

/// The label audit buckets use for a member row.
fn member_label(member: &MemberRow) -> String {
    let source = member.held_by.first().map(String::as_str).or_else(|| {
        member
            .advanced
            .first()
            .and_then(|advanced| advanced.split_once(" advanced to "))
            .map(|(branch, _)| branch)
    });
    source.map_or_else(
        || member.commit.short().to_owned(),
        |source| format!("{source}@{}", member.commit.short()),
    )
}

/// Gather the parents, holders, and optional content audit for a named release.
///
/// `ledger` is the fork's ledger: the writes it records under the name, with
/// the publish remote's commit of it, are the carriers a row labels
/// ([`crate::release_model::carriers`]). `verify` replays the content
/// (`--verify`) against no previous release: a member whose change the tree
/// does not hold reads as missing, however the release came to drop it.
///
/// A release of a stacked series that has a previous release reads as its
/// previous release, on a row of its own, and its members
/// ([`stacked_release`]); `verify` replays each member from its fork point
/// with the previous release ([`audit_cut_from`]). A release the
/// stacked reading refuses is a problem, and its rows are its parents.
pub fn gather_members(
    opened: &Repo,
    fork: &Fork<'_>,
    name: &str,
    (ledger, verify): (&[Entry], bool),
) -> anyhow::Result<MembersReport> {
    let entry = fork.entry;
    let commit = opened.resolve_commit(name)?;
    let parents = opened.parents_of(commit.as_str())?;
    let parent_count = parents.len();
    let trunk_name = entry.trunk_ref();
    let mut problems = Vec::new();
    let trunk = match opened.resolve_commit(&trunk_name) {
        Ok(trunk) => Some(trunk),
        Err(error) => {
            problems.push(format!(
                "cannot resolve upstream trunk {trunk_name}: {error}"
            ));
            None
        }
    };
    let trunks = trunk_positions(opened, entry)?;
    let tips = opened.bookmark_tips()?;
    let branches = carried_from_tips(&tips, entry.trunk(), &entry.release_scheme());
    let successions = BranchSuccessions::of(opened, &trunks, &branches)?;
    // The ledger records writes under the name, never under a remote's ref.
    let release = BookmarkRef::parse(name).branch().clone();
    let published = tips.get(&BookmarkRef::Remote {
        branch: release.clone(),
        remote: RemoteName::new(entry.publish_remote()),
    });
    let stacked = stacked_release(
        opened,
        entry,
        (ledger, &tips, &trunks),
        (release.as_str(), published, &commit),
    )?;
    let refused = matches!(stacked, Some(Err(_)));
    let (members, previous) = match stacked {
        Some(Ok(reading)) => (
            stacked_rows(opened, &reading, (&tips, trunk.as_ref()), &successions)?,
            reading.previous,
        ),
        stacked => {
            if let Some(Err(unexplained)) = stacked {
                problems.push(format!("{name}: {unexplained}"));
            }
            let rows = parent_rows(
                opened,
                parents,
                (release.as_str(), ledger, published),
                (trunk.as_ref(), &successions),
            )?;
            (rows, None)
        }
    };
    let audit = if verify && !refused {
        trunk
            .as_ref()
            .map(|trunk| {
                let members: Vec<(String, CommitId)> = members
                    .iter()
                    .filter(|member| is_member(member))
                    .map(|member| (member_label(member), member.commit.clone()))
                    .collect();
                audit_cut_from(
                    &fork.checkout.path,
                    &members,
                    CutSubject::Committed(&commit),
                    (
                        AuditContext {
                            previous: None,
                            trunk,
                        },
                        previous.as_ref().map(|previous| &previous.commit),
                    ),
                )
            })
            .transpose()?
    } else {
        None
    };
    Ok(MembersReport {
        repo: fork.checkout.path.display().to_string(),
        release: name.to_owned(),
        commit,
        parent_count,
        members,
        audit,
        notes: Vec::new(),
        problems,
    })
}

/// Render the gathered parent state and optional audit in the cut command's words.
pub fn render_members(report: &MembersReport) -> String {
    use crate::ledger::inline_human_text;
    // The release, its holders, its advancing branches and the audit's
    // labels are names read from the repository; git takes a C1 character
    // in a ref name, and jj imports such a ref as a remote bookmark.
    let mut lines = vec![format!(
        "{} @ {} — {} parents",
        inline_human_text(&report.release),
        report.commit.short(),
        report.parent_count
    )];
    let label =
        crate::release_model::carrier_label(BookmarkRef::parse(&report.release).branch().as_str());
    for member in &report.members {
        let mut description = inline_human_text(&member.held_by.join(", "));
        if !member.advanced.is_empty() {
            if !description.is_empty() {
                description.push_str(", ");
            }
            description.push_str(&inline_human_text(&member.advanced.join(", ")));
        }
        let previous = member
            .previous_release
            .as_deref()
            .map(previous_release_label);
        let tag = previous
            .as_deref()
            .or_else(|| member.carrier.then_some(label.as_str()));
        if let Some(tag) = tag {
            description = if description.is_empty() {
                inline_human_text(tag)
            } else {
                format!("{} ({description})", inline_human_text(tag))
            };
        }
        if description.is_empty() {
            description.push_str("bare commit — nothing holds it");
        }
        if member.base_parent {
            description.push_str(" (base parent)");
        }
        lines.push(format!("- {} {description}", member.commit.short()));
    }
    if let Some(audit) = &report.audit {
        for name in &audit.carried {
            lines.push(carried_row(name, CarriedFrom::Unledgered));
        }
        for name in &audit.inconclusive {
            lines.push(format!(
                "  {}: content check inconclusive (replay conflicted; \
                 re-check after resolving the cut's conflicts)",
                inline_human_text(name)
            ));
        }
        for member in report.members.iter().filter(|member| is_member(member)) {
            let name = member_label(member);
            if !audit.carried.contains(&name)
                && !audit.inconclusive.contains(&name)
                && !audit.missing.contains(&name)
            {
                lines.push(format!(
                    "  {}: carried (replay empty)",
                    inline_human_text(&name)
                ));
            }
        }
        for name in &audit.missing {
            lines.push(format!(
                "  !! {}: the cut tree is missing or diverges from the member's content",
                inline_human_text(name)
            ));
        }
        // Paths jj's diff named, so each goes out escaped.
        for file in &audit.unexplained {
            lines.push(format!(
                "  !! {}: changed between the previous release and this cut \
                 with no member or trunk explaining it",
                inline_human_text(file)
            ));
        }
    }
    // A note quotes jj, the forge or a consumer's pin files; the report keeps
    // the raw text for `--json`, and this line is the one a terminal reads.
    lines.extend(
        report
            .notes
            .iter()
            .map(|note| format!("! {}", inline_human_text(note))),
    );
    // A problem can name a path from disk or another machine's ledger ref.
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("!! {}", crate::ledger::problem_human_text(problem))),
    );
    lines.join("\n")
}

pub fn render(plan: &Plan) -> String {
    let mut lines: Vec<String> = plan
        .problems
        .iter()
        .map(|problem| format!("!! {}", crate::ledger::problem_human_text(problem)))
        .chain(
            plan.notes
                .iter()
                .map(|note| format!("! {}", crate::ledger::inline_human_text(note))),
        )
        .collect();
    let Some(release) = &plan.release else {
        return lines.join("\n");
    };

    // The release and the parents' bookmarks are names read from the
    // repository, which a remote can give a C1 character.
    lines.push(format!(
        "{}: {}",
        plan.repo,
        crate::ledger::inline_human_text(release)
    ));
    let stacked = plan
        .base_findings
        .iter()
        .filter(|finding| finding.kind == FindingKind::StackedHistory)
        .count();
    lines.push(if stacked == 0 {
        format!("  {} parent(s), flat", plan.parents.len())
    } else {
        format!(
            "  {} parent(s), {stacked} stacked on a prior merge",
            plan.parents.len()
        )
    });
    lines.extend(plan.parent_lines(release));

    if plan.stale.is_empty() {
        lines.push("  every parent is still its branch tip".to_owned());
    } else {
        lines.push(format!("  {} stale parent(s):", plan.stale.len()));
        for finding in &plan.stale {
            lines.push(format!(
                "    {}",
                crate::ledger::inline_human_text(&finding.detail)
            ));
        }
    }
    for finding in &plan.base_findings {
        lines.push(format!(
            "  !! {}",
            crate::ledger::inline_human_text(&finding.detail)
        ));
    }

    // The verdict line below says "nothing pins this release" on its own; a
    // heading over an empty list added a line about a consumer that may not exist.
    if !plan.pins.is_empty() {
        lines.push("  pinned by:".to_owned());
        lines.push(crate::pins::render(&plan.pins));
    }
    // A consumer that could not be consulted may hold the pin the verdict would
    // deny; the census refuses the same no-pin claim after a failed scan.
    lines.push(if plan.pins_unknown {
        "  pinned-ness unknown: a consumer could not be consulted (see above)".to_owned()
    } else {
        match repair_effect(
            &plan.pins,
            BookmarkRef::parse(release).branch(),
            plan.published.as_ref(),
        ) {
            RepairEffect::RepairInPlace => {
                "  at least one consumer follows the branch: repair in place, no new dated name"
                    .to_owned()
            }
            RepairEffect::NewDatedName => {
                "  every pin of this release is frozen: editing it reaches nobody; the next cut \
             needs a new dated suffix"
                    .to_owned()
            }
            // The list above may name pins of something else — an older cut, a
            // tag of the consumer's own — so the verdict says which claim it makes.
            RepairEffect::Unpinned if plan.pins.is_empty() => {
                "  nothing pins this release: either is safe".to_owned()
            }
            RepairEffect::Unpinned => {
                "  none of these pins is of this release: either is safe".to_owned()
            }
        }
    });
    lines.push(
        "  planning by default. `knives release cut [name]` names a new cut of this \
           composition verbatim; `include`, `drop` and `advance` edit it. Nothing here \
           ever pushes."
            .to_owned(),
    );
    lines.join("\n")
}

/// Findings mean act; notes mean we could not answer. A command that reports a
/// problem in its text and still exits zero lets a CI gate go green on a broken
/// forge login or an unopenable repository.
pub const fn exit_for(plan: &Plan) -> Exit {
    if !plan.problems.is_empty() {
        return Exit::Incomplete;
    }
    if plan.stale.is_empty() && plan.base_findings.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

#[cfg(test)]
mod reap_enumeration_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ids::{BookmarkRef, BranchName, CommitId, RemoteName};

    fn local(name: &str, commit: &str) -> (BookmarkRef, CommitId) {
        (
            BookmarkRef::Local(BranchName::new(name)),
            CommitId::new(commit),
        )
    }

    fn remote(name: &str, remote: &str, commit: &str) -> (BookmarkRef, CommitId) {
        (
            BookmarkRef::Remote {
                branch: BranchName::new(name),
                remote: RemoteName::new(remote),
            },
            CommitId::new(commit),
        )
    }

    #[test]
    fn every_ref_of_a_superseded_dated_name_is_enumerated_on_any_remote_but_upstream() {
        // Given: two dated cuts with refs scattered across remotes (the shape a
        // pre-knives fork accumulates), upstream's own semver branch, and a work branch.
        let tips: BookmarkTips = [
            local("release/2026-08-04", "aaa"),
            remote("release/2026-08-04", "release", "aaa"),
            remote("release/2026-08-04", "publish2", "aaa"),
            remote("release/2026-08-04", "git", "aaa"),
            local("release/2026-08-05", "bbb"),
            remote("release/2026-08-05", "release", "bbb"),
            remote("release/0.3.190", "upstream", "ccc"),
            remote("release/2026-07-01", "upstream", "ddd"),
            local("feat/x", "eee"),
        ]
        .into_iter()
        .collect();

        let superseded = superseded_dated_releases(&tips, &ReleaseScheme::Dated, "release");
        let names: Vec<String> = superseded.iter().map(|(r, _)| r.to_string()).collect();

        // Then: only the older dated name, on every remote except upstream and git.
        assert_eq!(
            names,
            vec![
                "release/2026-08-04".to_owned(),
                "release/2026-08-04@publish2".to_owned(),
                "release/2026-08-04@release".to_owned(),
            ]
        );
    }

    #[test]
    fn an_upstream_dated_ref_never_votes_on_which_cut_is_newest() {
        // Given: upstream carries a dated-shaped name NEWER than our newest cut.
        // It must neither appear in the output nor make OUR newest look superseded
        // — the reaper would otherwise forget and abandon the live release.
        let tips: BookmarkTips = [
            local("release/2026-08-05", "aaa"),
            remote("release/2026-08-05", "release", "aaa"),
            remote("release/2026-09-01", "upstream", "bbb"),
            local("release/2026-08-04", "ccc"),
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = superseded_dated_releases(&tips, &ReleaseScheme::Dated, "release")
            .iter()
            .map(|(r, _)| r.to_string())
            .collect();
        assert_eq!(names, vec!["release/2026-08-04".to_owned()]);
    }

    #[test]
    fn a_third_remotes_dated_ref_never_votes_on_which_cut_is_newest() {
        // Given: a mirror/colleague remote carries a dated name newer than our
        // live cut. It must not outrank the live release (the vote trusts only
        // local/origin/release, like newest_release); its own stale ref is still
        // enumerated for cleanup.
        let tips: BookmarkTips = [
            local("release/2026-08-05", "aaa"),
            remote("release/2026-08-05", "release", "aaa"),
            remote("release/2026-09-01", "publish2", "bbb"),
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = superseded_dated_releases(&tips, &ReleaseScheme::Dated, "release")
            .iter()
            .map(|(r, _)| r.to_string())
            .collect();
        assert_eq!(names, vec!["release/2026-09-01@publish2".to_owned()]);
    }

    #[test]
    fn superseded_refs_are_sorted_by_name_before_ref_kind() {
        // Given: two superseded names, each represented locally and on a remote.
        let tips: BookmarkTips = [
            local("release/2026-08-04", "aaa"),
            remote("release/2026-08-04", "release", "aaa"),
            local("release/2026-08-03", "bbb"),
            remote("release/2026-08-03", "release", "bbb"),
            local("release/2026-08-05", "ccc"),
        ]
        .into_iter()
        .collect();

        // When: the reaper candidates are enumerated.
        let names: Vec<String> = superseded_dated_releases(&tips, &ReleaseScheme::Dated, "release")
            .iter()
            .map(|(r, _)| r.to_string())
            .collect();

        // Then: refs for each dated name remain adjacent.
        assert_eq!(
            names,
            vec![
                "release/2026-08-03".to_owned(),
                "release/2026-08-03@release".to_owned(),
                "release/2026-08-04".to_owned(),
                "release/2026-08-04@release".to_owned(),
            ]
        );
    }

    #[test]
    fn the_newest_dated_name_is_never_superseded_even_when_only_remote() {
        // The newest cut may exist only as a remote ref in a fresh clone.
        let tips: BookmarkTips = [
            local("release/2026-08-04", "aaa"),
            remote("release/2026-08-05.2", "release", "bbb"),
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = superseded_dated_releases(&tips, &ReleaseScheme::Dated, "release")
            .iter()
            .map(|(r, _)| r.to_string())
            .collect();
        assert_eq!(names, vec!["release/2026-08-04".to_owned()]);
    }

    #[test]
    fn each_series_reaps_only_its_own_superseded_cuts() {
        // Given: two default-series cuts and two series-9.2 cuts, the 9.2 ones
        // dated later than every default cut.
        let tips: BookmarkTips = [
            local("release/2026-10-19", "aaa"),
            local("release/2026-10-20", "bbb"),
            local("release/9.2/2026-10-21", "ccc"),
            local("release/9.2/2026-10-22", "ddd"),
        ]
        .into_iter()
        .collect();
        let names = |scheme: &ReleaseScheme| -> Vec<String> {
            superseded_dated_releases(&tips, scheme, "origin")
                .iter()
                .map(|(r, _)| r.to_string())
                .collect()
        };
        // Then: each series keeps its own newest cut live; neither series'
        // later dates supersede the other's.
        assert_eq!(names(&ReleaseScheme::Dated), vec!["release/2026-10-19"]);
        assert_eq!(
            names(&ReleaseScheme::Series(crate::ids::SeriesName::new("9.2"))),
            vec!["release/9.2/2026-10-21"]
        );
        // And: every series' cut is a release commit, never an orphan.
        assert_eq!(our_dated_release_commits(&tips, "origin").len(), 4);
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ids::BranchName;

    fn pin(kind: PinKind) -> Pin {
        Pin {
            file: "pyproject.toml".to_owned(),
            line: 1,
            reference: "release/2026-07-28".to_owned(),
            kind,
            locked: None,
            on_scheme: true,
            source: String::new(),
        }
    }

    #[test]
    fn one_following_consumer_means_repair_in_place() {
        // A needless dated name burns the name and forces a re-pin nobody wanted.
        let pins = vec![pin(PinKind::Frozen), pin(PinKind::Follows)];
        assert_eq!(
            repair_effect(&pins, &BranchName::new("release/2026-07-28"), None),
            RepairEffect::RepairInPlace
        );
    }

    #[test]
    fn all_frozen_means_a_new_dated_name() {
        assert_eq!(
            repair_effect(
                &[pin(PinKind::Frozen)],
                &BranchName::new("release/2026-07-28"),
                None
            ),
            RepairEffect::NewDatedName
        );
    }

    #[test]
    fn nothing_pinning_it_leaves_the_choice_open() {
        assert_eq!(
            repair_effect(&[], &BranchName::new("release/2026-07-28"), None),
            RepairEffect::Unpinned
        );
    }

    #[test]
    fn a_pin_frozen_on_another_release_does_not_freeze_this_one() {
        // The consumer sits frozen on an older cut; editing the release in hand
        // reaches it neither way, so it must not block the edit. Judged over every
        // pin, a fork with one such consumer could edit no release at all.
        let pins = [pin(PinKind::Frozen)];
        assert_eq!(
            repair_effect(&pins, &BranchName::new("release/2026-08-31"), None),
            RepairEffect::Unpinned
        );
        assert_eq!(
            repair_effect(
                &pins,
                BookmarkRef::parse("release/2026-07-28@release").branch(),
                None
            ),
            RepairEffect::NewDatedName,
            "the publish remote's view of the same release is the same release"
        );
    }

    #[test]
    fn an_off_scheme_pin_alone_leaves_the_repair_choice_open() {
        // A consumer pinned at its own tag is not consuming releases: repairing in
        // place cannot reach it and a new dated name would not either — also when
        // the release has a published commit to compare against.
        let mut off_scheme = pin(PinKind::Frozen);
        off_scheme.on_scheme = false;
        off_scheme.reference = "acme-pin-0.4.47.dev7".to_owned();
        let published = CommitId::new(PUBLISHED);
        assert_eq!(
            repair_effect(
                &[off_scheme],
                &BranchName::new("acme-pin-0.4.47.dev7"),
                Some(&published)
            ),
            RepairEffect::Unpinned
        );
    }

    #[test]
    fn a_following_pin_last_resolved_at_the_published_commit_is_not_frozen_there() {
        // `?branch=integration#<sha>`: the lock says where the follower resolved
        // last time, not where it is held. Counting it as a pin of this release
        // would let one such consumer, next to a genuinely frozen one, turn the
        // verdict back into "repair in place" — the edit allowed, reaching nobody.
        let published = CommitId::new(PUBLISHED);
        let release = BranchName::new("release/2026-09-11");
        let mut follower = sha_pin("integration");
        follower.kind = PinKind::Follows;
        follower.locked = Some(PUBLISHED.to_owned());
        assert_eq!(
            repair_effect(&[follower.clone()], &release, Some(&published)),
            RepairEffect::Unpinned
        );
        assert_eq!(
            repair_effect(&[sha_pin(PUBLISHED), follower], &release, Some(&published)),
            RepairEffect::NewDatedName
        );
    }

    const PUBLISHED: &str = "c2638ddd8d5dd9127cb8b9ba2c2e349a3b79782b";

    /// A consumer that pins the fork by raw commit, the way a lockfile written
    /// from an installed commit does: `?rev=<sha>`.
    fn sha_pin(reference: &str) -> Pin {
        Pin {
            file: "uv.lock".to_owned(),
            line: 2008,
            reference: reference.to_owned(),
            kind: PinKind::Frozen,
            locked: None,
            on_scheme: false,
            source: String::new(),
        }
    }

    #[test]
    fn a_consumer_frozen_on_the_published_commit_pins_the_release() {
        // The consumer never names the release: its lockfile carries the exact
        // commit the publish remote holds for it. That is a frozen pin of this
        // release, so editing it in place reaches nobody and the next cut needs
        // a new dated name. Judged by name alone, the same fork read as unpinned
        // and both gates were wrong at once.
        let published = CommitId::new(PUBLISHED);
        let release = BranchName::new("release/2026-09-11");
        assert_eq!(
            repair_effect(&[sha_pin(PUBLISHED)], &release, Some(&published)),
            RepairEffect::NewDatedName
        );
        assert_eq!(
            repair_effect(&[sha_pin("c2638ddd")], &release, Some(&published)),
            RepairEffect::NewDatedName,
            "an abbreviated commit is the same pin"
        );
    }

    #[test]
    fn a_consumer_frozen_on_another_commit_does_not_freeze_this_release() {
        // Frozen on some other commit — an older cut, a tag of its own — the
        // consumer is reached neither by an edit nor by a new name.
        let published = CommitId::new(PUBLISHED);
        let release = BranchName::new("release/2026-09-11");
        assert_eq!(
            repair_effect(
                &[sha_pin("55a837ea6c1f0d2b3a4c5d6e7f8091a2b3c4d5e6")],
                &release,
                Some(&published)
            ),
            RepairEffect::Unpinned
        );
        assert_eq!(
            repair_effect(&[sha_pin("c2638d")], &release, Some(&published)),
            RepairEffect::Unpinned,
            "fewer than seven hex digits is not a commit pin"
        );
        assert_eq!(
            repair_effect(&[sha_pin(PUBLISHED)], &release, None),
            RepairEffect::Unpinned,
            "with no published commit to compare against, a raw commit pins nothing knowable"
        );
    }

    #[test]
    fn a_lock_fragment_at_the_published_commit_counts_whatever_the_reference_names() {
        // A lockfile can carry `?rev=<tag>#<sha>`: the reference is a tag of the
        // consumer's own, but the lock resolved it to this release's commit.
        let published = CommitId::new(PUBLISHED);
        let mut tagged = sha_pin("acme-pin-0.4.47.dev7");
        tagged.locked = Some("c2638ddd8d5d".to_owned());
        assert_eq!(
            repair_effect(
                &[tagged],
                &BranchName::new("release/2026-09-11"),
                Some(&published)
            ),
            RepairEffect::NewDatedName
        );
    }

    #[test]
    fn carried_branches_excludes_the_configured_trunk_not_the_name_main() {
        // A fork of a dev-trunk upstream may carry a branch literally named main;
        // that branch is work, and dev is the one that is not.
        // (Constructed through the pure filter, mirroring maintained_branches.)
        let tips: crate::detect::BookmarkTips = [
            (
                BookmarkRef::Local(BranchName::new("dev")),
                CommitId::new("aaa"),
            ),
            (
                BookmarkRef::Local(BranchName::new("main")),
                CommitId::new("bbb"),
            ),
            (
                BookmarkRef::Local(BranchName::new("feat/x")),
                CommitId::new("ccc"),
            ),
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = carried_from_tips(&tips, "dev", &crate::ids::ReleaseScheme::Dated)
            .into_iter()
            .map(|(branch, _)| branch)
            .collect();
        assert!(!names.contains(&"dev".to_owned()));
        assert!(names.contains(&"main".to_owned()));
    }

    #[test]
    fn a_fixed_release_branch_is_a_cut_not_carried_cargo() {
        // Given: a fixed integration cut alongside the configured trunk and feature work.
        let fixed = crate::ids::ReleaseScheme::Fixed(BranchName::new("integration"));
        let tips: crate::detect::BookmarkTips = [
            (
                BookmarkRef::Local(BranchName::new("integration")),
                CommitId::new("aaa"),
            ),
            (
                BookmarkRef::Local(BranchName::new("feat/x")),
                CommitId::new("bbb"),
            ),
            (
                BookmarkRef::Local(BranchName::new("dev")),
                CommitId::new("ccc"),
            ),
        ]
        .into_iter()
        .collect();
        // When: the next cut's carried branches are gathered.
        let carried = carried_from_tips(&tips, "dev", &fixed);
        // Then: only the feature is cargo; the fixed branch advances in place.
        assert_eq!(carried, vec![("feat/x".to_owned(), CommitId::new("bbb"))]);
    }

    /// Nothing in a release report that this process did not write — a path a
    /// jj diff named, a note quoting the forge, a finding detail built from jj
    /// text, a consumer's pin — may carry a terminal control sequence out to
    /// the reader. The plan keeps the raw text for `--json`; this is the line
    /// a terminal reads.
    #[test]
    fn a_control_character_in_a_plan_reaches_the_terminal_escaped() {
        let raw = "a\u{7}b";
        let finding = |detail: &str| {
            Finding::new(
                FindingKind::StackedHistory,
                Subject::Branch(BranchName::new("feat/alpha")),
                detail,
            )
        };
        let plan = Plan {
            repo: "work".to_owned(),
            release: Some("release/2026-07-28".to_owned()),
            notes: vec![raw.to_owned()],
            stale: vec![finding(raw)],
            base_findings: vec![finding(raw)],
            pins: vec![Pin {
                file: raw.to_owned(),
                reference: raw.to_owned(),
                ..pin(PinKind::Frozen)
            }],
            ..Plan::default()
        };
        let rendered = render(&plan);
        assert!(!rendered.contains('\u{7}'), "{rendered}");
        assert_eq!(
            rendered.matches("a\u{fffd}b").count(),
            5,
            "note, stale detail, base finding, pin file and pin reference: {rendered}"
        );
        assert_eq!(plan.notes[0], raw, "the plan itself keeps the raw note");
    }

    #[test]
    fn a_control_character_in_a_members_report_reaches_the_terminal_escaped() {
        let raw = "src/a\u{7}b.rs";
        let report = MembersReport {
            repo: "work".to_owned(),
            release: "release/2026-07-28".to_owned(),
            commit: CommitId::new(PUBLISHED),
            parent_count: 0,
            members: Vec::new(),
            audit: Some(CutAudit {
                unexplained: vec![raw.to_owned()],
                ..CutAudit::default()
            }),
            notes: vec![raw.to_owned()],
            problems: Vec::new(),
        };
        let rendered = render_members(&report);
        assert!(!rendered.contains('\u{7}'), "{rendered}");
        assert_eq!(
            rendered.matches("src/a\u{fffd}b.rs").count(),
            2,
            "the unexplained diff path and the note: {rendered}"
        );
    }

    #[test]
    fn a_control_character_in_a_conflicted_path_reaches_the_terminal_escaped() {
        let rendered = conflict_guidance("release/2026-08-30", &["src/a\u{7}b.rs".to_owned()], &[]);
        assert!(!rendered.contains('\u{7}'), "{rendered}");
        assert!(rendered.contains("src/a\u{fffd}b.rs"), "{rendered}");
    }
}

/// A cut that has been checked but not yet made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    pub name: String,
    pub parents: Vec<CommitId>,
    /// Where each parent came from: the branch holding it, the trunk it
    /// descends from, or its own id when nothing else names it. Records
    /// provenance and pins nothing: a jj octopus's parents are already
    /// specific commits.
    pub provenance: Vec<(CommitId, String)>,
}

impl Cut {
    /// The cut for `carried`. A commit two bookmarks hold — a branch and an
    /// anchor another agent left at its tip — is one parent, named twice in
    /// provenance.
    pub fn from_carried(name: String, carried: &[(String, CommitId)]) -> Self {
        let mut parents: Vec<CommitId> = Vec::with_capacity(carried.len());
        for (_, commit) in carried {
            if !parents.contains(commit) {
                parents.push(commit.clone());
            }
        }
        Self {
            name,
            parents,
            provenance: carried
                .iter()
                .map(|(branch, commit)| (commit.clone(), branch.clone()))
                .collect(),
        }
    }

    /// The message a cut carries, provenance included.
    pub fn message(&self) -> String {
        let mut lines = vec![format!("release: {}", self.name), String::new()];
        for (commit, source) in &self.provenance {
            lines.push(format!("parent {} from {source}", commit.as_str()));
        }
        lines.join("\n")
    }
}

/// Name each parent for the release description: the branch holding it, the
/// trunk it descends from, or its own id when nothing else does.
pub fn parent_sources(
    repo: &Repo,
    entry: &RepoEntry,
    scheme: &ReleaseScheme,
    parents: &[CommitId],
) -> anyhow::Result<Vec<(String, CommitId)>> {
    let tips = repo.bookmark_tips()?;
    let carried = carried_from_tips(&tips, entry.trunk(), scheme);
    let trunk_tip = repo.resolve_commit(&entry.trunk_ref()).ok();
    let mut sources = Vec::new();
    for commit in parents {
        let named = carried
            .iter()
            .find(|(_, tip)| tip == commit)
            .map(|(branch, _)| branch.clone());
        let source = if let Some(named) = named {
            named
        } else if let Some(trunk) = &trunk_tip
            && repo.is_ancestor(commit, trunk)?
        {
            entry.trunk_ref()
        } else {
            commit.short().to_owned()
        };
        sources.push((source, commit.clone()));
    }
    Ok(sources)
}

/// The description of a release after a write.
///
/// A cut's message from the parents' `provenance`, so an edited or rebased
/// release reads exactly like a fresh cut, then `delta` — what the write
/// changed — as its own paragraph.
pub fn composition_message(name: &str, provenance: &[(String, CommitId)], delta: &str) -> String {
    format!(
        "{}\n\n{delta}",
        Cut::from_carried(name.to_owned(), provenance).message()
    )
}

/// The post-construction checks that determine whether a cut is safe to name.
#[derive(Debug, Default, serde::Serialize)]
pub struct CutAudit {
    pub missing: Vec<String>,
    pub unexplained: Vec<String>,
    pub inconclusive: Vec<String>,
    /// Members the cut diverges from exactly as the previous release already
    /// did. Reported, never refused — the audit charges a cut only with
    /// divergence it introduces. Whether that divergence is a recorded
    /// resolution is the caller's question: each row says how the previous
    /// release stands to its recorded writes ([`carried_row`]).
    pub carried: Vec<String>,
}

impl CutAudit {
    pub const fn passed(&self) -> bool {
        self.missing.is_empty() && self.unexplained.is_empty()
    }
}

/// The baseline commits used to explain a cut audit's file-level drift.
#[derive(Debug, Clone, Copy)]
pub struct AuditContext<'a> {
    pub previous: Option<&'a CommitId>,
    pub trunk: &'a CommitId,
}

/// Workspaces knives made for a branch that is gone and that nobody holds.
///
/// A workspace is named when its name is the workspace of a branch `recorded`
/// (the branches the fork's ledger names: knives started or noted them), and
/// no branch in `live` maps to it: every bookmark name, local or on any remote,
/// and every branch a live claim holds. A workspace knives never made, a held
/// one (a `knives start` of a release whose name a reap forgot), and the
/// default workspace are never named: telling someone to forget another
/// agent's workspace can destroy its work.
pub fn workspaces_to_clean(
    workspaces: &[String],
    recorded: &[String],
    live: &[String],
) -> Vec<String> {
    let flat = |branches: &[String]| -> BTreeSet<String> {
        branches
            .iter()
            .map(|branch| crate::commands::wip::workspace_for(branch))
            .collect()
    };
    let (recorded, live) = (flat(recorded), flat(live));
    workspaces
        .iter()
        .filter(|name| {
            name.as_str() != "default" && recorded.contains(*name) && !live.contains(*name)
        })
        .cloned()
        .collect()
}

/// Build the candidate cut — in a scratch transaction the audit can read and
/// a failed audit simply drops — and verify it has exactly the parents asked
/// for.
///
/// `Some((previous, carriers))` duplicates the previous release onto the new
/// parent set, preserving its recorded conflict resolutions, with `carriers`,
/// its parents that are earlier writes of its name, left out of what the
/// duplicate measures it against ([`jj::CutSpec::carriers`]). Adding a branch
/// or advancing the base therefore avoids replaying old conflicts; dropping
/// an entangled branch surfaces a focused conflict rather than silently
/// retaining its content. `None` builds a fresh flat merge. Both paths
/// produce a flat octopus of exactly `request.parents`.
/// The parent-count guard catches a silent failure: a cut that dropped a parent
/// looks exactly like one that did not, until work goes missing downstream.
///
/// Nothing is published until [`publish_cut`], and nothing ever pushes.
pub fn candidate_cut(
    repo: &Path,
    request: &Cut,
    previous: Option<(&CommitId, &[CommitId])>,
) -> anyhow::Result<jj::Candidate> {
    let candidate = jj::candidate_release(
        repo,
        jj::CutSpec {
            source: previous.map(|(previous, _)| previous.clone()),
            parents: request.parents.clone(),
            carriers: previous.map_or_else(Vec::new, |(_, carriers)| carriers.to_vec()),
            message: request.message(),
        },
    )?;
    anyhow::ensure!(
        candidate.parent_count() == request.parents.len(),
        "cut {} came out with {} parents, expected {}; refusing to name it",
        request.name,
        candidate.parent_count(),
        request.parents.len()
    );
    Ok(candidate)
}

/// Rebuild the audited candidate and point the release name at it, as ONE
/// published operation.
///
/// Dated cuts retain ordinary bookmark-movement protection because each name
/// records a new release. Fixed cuts deliberately move their existing release
/// name, which may already exist on the remote. `earlier` are the earlier
/// writes of the release the cut duplicates and `publish_remote` where the
/// name publishes, for what the cut leaves a head nothing holds
/// ([`jj::Candidate::publish`]).
pub fn publish_cut(
    candidate: jj::Candidate,
    name: &str,
    scheme: &ReleaseScheme,
    (earlier, publish_remote): (&[CommitId], Option<&str>),
) -> anyhow::Result<jj::PublishedCut> {
    let motion = match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => jj::BookmarkMotion::ForwardOnly,
        ReleaseScheme::Fixed(_) => jj::BookmarkMotion::Anywhere,
    };
    Ok(candidate.publish(
        (name, motion),
        (earlier, publish_remote),
        &format!("knives: cut {name}"),
    )?)
}

/// The cut being audited: an unpublished candidate in its scratch transaction,
/// or a committed commit (how a published cut is re-examined).
#[derive(Debug)]
pub enum CutSubject<'a> {
    Candidate(&'a mut jj::Candidate),
    Committed(&'a CommitId),
}

impl CutSubject<'_> {
    fn conflicted_files(&mut self, repo: &Path) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Candidate(candidate) => Ok(candidate.conflicted_files()?),
            Self::Committed(cut) => Ok(crate::jj::conflicted_files(repo, cut.as_str())?),
        }
    }

    fn replay_outcome(
        &mut self,
        repo: &Path,
        base: &str,
        revision: &str,
    ) -> anyhow::Result<RebaseOutcome> {
        match self {
            Self::Candidate(candidate) => Ok(candidate.replay_outcome(base, revision)?),
            Self::Committed(cut) => Ok(crate::jj::probe_net_diff(
                repo,
                base,
                revision,
                cut.as_str(),
            )?),
        }
    }

    fn changed_files_since(
        &mut self,
        repo: &Path,
        previous: &CommitId,
    ) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Candidate(candidate) => Ok(candidate.changed_files_since(previous.as_str())?),
            Self::Committed(cut) => Ok(crate::jj::changed_files_between(
                repo,
                previous.as_str(),
                cut.as_str(),
            )?),
        }
    }
}

/// Verify the cut actually contains what it merged (spec 1.3).
///
/// For each member, a scratch child of `trunk` carrying the member tip's tree
/// — a synthetic commit whose diff is `trunk..member_tip` — is replayed onto
/// the cut.
/// An empty replay means its hunks are present; a clean, non-empty replay means
/// the cut silently lacks them. A conflicted replay is inconclusive only when
/// the cut itself has unresolved conflicts; otherwise its tree diverges from the
/// member and fails the audit.
/// Changes from the previous release that no member or trunk explains are
/// merge-invented drift.
///
/// Auditing a [`CutSubject::Candidate`] reads a merge that no other observer
/// can see and that a failure simply drops — nothing to compensate, nothing
/// stranded by a crash.
pub fn audit_cut(
    repo: &Path,
    members: &[(String, CommitId)],
    subject: CutSubject<'_>,
    context: AuditContext<'_>,
) -> anyhow::Result<CutAudit> {
    audit_cut_from(repo, members, subject, (context, None))
}

/// [`audit_cut`], of a release of a stacked series when `stacked_on` is the
/// previous release it stacks on ([`crate::release_model::read_release`]).
///
/// Such a release carries its previous release whole, and a member may
/// stack on it. So each member replays from its fork point with the previous
/// release, the merge base of the two, not from `trunk`: the previous
/// release's own content is never charged to a member. The drift is what
/// changed since the previous release, not since `previous`. The previous
/// release is no member, and the caller does not list it among `members`.
/// `None` is [`audit_cut`].
pub fn audit_cut_from(
    repo: &Path,
    members: &[(String, CommitId)],
    mut subject: CutSubject<'_>,
    (context, stacked_on): (AuditContext<'_>, Option<&CommitId>),
) -> anyhow::Result<CutAudit> {
    let mut audit = CutAudit::default();
    let cut_is_conflicted = !subject.conflicted_files(repo)?.is_empty();
    let stacked = stacked_on
        .map(|previous| Repo::open(repo).map(|opened| (opened, previous)))
        .transpose()?;
    for (name, tip) in members {
        let fork_point = stacked
            .as_ref()
            .map(|(opened, previous)| opened.fork_point(tip, previous))
            .transpose()?;
        let base = fork_point.as_ref().unwrap_or(context.trunk);
        match subject.replay_outcome(repo, base.as_str(), tip.as_str())? {
            RebaseOutcome::Empty => {}
            RebaseOutcome::Conflicted if cut_is_conflicted => audit.inconclusive.push(name.clone()),
            RebaseOutcome::CleanNonEmpty | RebaseOutcome::Conflicted => {
                // Divergence the release held against already carried is that
                // release's resolution, carried forward, not a loss: refusing
                // it would make every release with a hand-resolved conflict
                // fail its own re-cut forever. Whether that resolution is
                // recorded is the caller's question, not this audit's: a hand
                // edit nothing recorded may have dropped a member's change, so
                // it is `knives release adopt` that measures it before a cut.
                let carried_before = match context.previous {
                    Some(previous) => !matches!(
                        crate::jj::probe_net_diff(
                            repo,
                            base.as_str(),
                            tip.as_str(),
                            previous.as_str(),
                        )?,
                        RebaseOutcome::Empty
                    ),
                    None => false,
                };
                if carried_before {
                    audit.carried.push(name.clone());
                } else {
                    audit.missing.push(name.clone());
                }
            }
        }
    }
    if let Some(previous) = stacked_on.or(context.previous) {
        let drifted = subject.changed_files_since(repo, previous)?;
        let mut explained = BTreeSet::new();
        for (_, tip) in members {
            explained.extend(crate::jj::changed_files_between(
                repo,
                previous.as_str(),
                tip.as_str(),
            )?);
        }
        explained.extend(crate::jj::changed_files_between(
            repo,
            previous.as_str(),
            context.trunk.as_str(),
        )?);
        audit.unexplained = drifted
            .into_iter()
            .filter(|file| !explained.contains(file))
            .collect();
    }
    Ok(audit)
}

/// Create and name a fresh cut for direct callers that do not need an audit.
/// A fresh merge replaces nothing, so it has no earlier write to hide.
pub fn cut(repo: &Path, request: &Cut, scheme: &ReleaseScheme) -> anyhow::Result<CommitId> {
    let candidate = candidate_cut(repo, request, None)?;
    Ok(publish_cut(candidate, &request.name, scheme, (&[], None))?.commit)
}

/// A recorded member whose carry check answered nothing either way.
#[derive(Debug, PartialEq, Eq)]
pub struct Unverified {
    /// Carried into the new cut event's evidence, so the next gate rechecks it.
    pub commit: CommitId,
    /// Rendered for a human: the branch still holding the commit when one does.
    pub name: String,
}

/// How a candidate cut relates to the previous cut's recorded composition.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompositionCheck {
    /// Recorded members whose content the candidate does not carry, rendered
    /// for a human: the branch still holding the commit when one does.
    pub dropped: Vec<String>,
    /// Members whose replay conflicted while the candidate itself is
    /// conflicted: unanswerable either way, exactly as the audit reports it.
    pub inconclusive: Vec<Unverified>,
    /// Recorded members carried by another commit with exactly their tree,
    /// reachable from a parent: the member rendered for a human, and the twin.
    pub twins: Vec<(String, CommitId)>,
}

/// What the composition gate compares: the cut's parents against the members
/// the previous cut's ledger event recorded.
#[derive(Debug)]
pub struct CompositionDelta<'a> {
    pub recorded: &'a RecordedCut,
    /// The cut's parents, for the identity and ancestry fast paths.
    pub parents: &'a [CommitId],
    /// The audit's base: what every current member forks from. A recorded
    /// member that is an ancestor of it entered the cut through the base — how
    /// a merge-landed member reads as carried after a rebase.
    pub base: &'a CommitId,
    /// The upstream trunk tip. A member reachable from it but not from the
    /// base or any parent landed upstream past what the cut ships: the trunk
    /// carries it, this cut does not, and that is a drop.
    pub trunk: &'a CommitId,
    pub tips: &'a BookmarkTips,
}

/// Recorded members of the previous cut that the cut subject does not carry.
///
/// Identity and ancestry account for a member that is still a parent, was
/// advanced past, or entered through the cut's base. A member whose change was
/// rewritten — `jj rebase` keeps the change id and mints a new commit — is
/// carried when a rewrite is reachable from a parent or the base, the same
/// succession `advance` recognises; the recorded commit itself is never an
/// ancestor again after a rebase. A member recreated outside jj with exactly
/// its tree is carried when that twin is reachable from a parent past the base
/// ([`Repo::same_tree_between`]), and the twin is named. The content replay
/// accounts for one that landed upstream as a squash — the same measure
/// [`audit_cut`] applies to current members, taken from the member's own fork
/// point so a moved base is never charged to the member. A member the trunk
/// reaches but the cut subject does not is dropped without a replay: its fork
/// point degenerates to the member itself, and that replay would read empty
/// without consulting the cut subject at all. A recorded commit this
/// repository cannot resolve counts as dropped: unverifiable must not read as
/// carried.
pub fn uncarried_recorded_members(
    repo: &Repo,
    path: &Path,
    subject: &mut CutSubject<'_>,
    delta: &CompositionDelta<'_>,
) -> anyhow::Result<CompositionCheck> {
    let mut check = CompositionCheck::default();
    let cut_is_conflicted = !subject.conflicted_files(path)?.is_empty();
    for member in &delta.recorded.members {
        if delta.parents.contains(member) {
            continue;
        }
        if repo.resolve_commit(member.as_str()).is_err() {
            check
                .dropped
                .push(format!("{} (not known to this repository)", member.short()));
            continue;
        }
        if reaches_any(repo, member, delta)? {
            continue;
        }
        let rewrites = repo.rewrites_of(member.as_str())?;
        let mut rewritten = false;
        for rewrite in &rewrites {
            if reaches_any(repo, rewrite, delta)? {
                rewritten = true;
                break;
            }
        }
        if rewritten {
            continue;
        }
        if let Some(twin) = repo.same_tree_between(member, delta.base, delta.parents)? {
            check
                .twins
                .push((recorded_member_name(member, delta.tips), twin));
            continue;
        }
        if repo.is_ancestor(member, delta.trunk)? {
            check.dropped.push(recorded_member_name(member, delta.tips));
            continue;
        }
        let base = repo.fork_point(member, delta.trunk)?;
        match subject.replay_outcome(path, base.as_str(), member.as_str())? {
            RebaseOutcome::Empty => {}
            RebaseOutcome::Conflicted if cut_is_conflicted => {
                check.inconclusive.push(Unverified {
                    commit: member.clone(),
                    name: recorded_member_name(member, delta.tips),
                });
            }
            RebaseOutcome::CleanNonEmpty | RebaseOutcome::Conflicted => {
                check.dropped.push(recorded_member_name(member, delta.tips));
            }
        }
    }
    Ok(check)
}

/// Whether the cut's base or any of its parents reaches `commit`.
fn reaches_any(
    repo: &Repo,
    commit: &CommitId,
    delta: &CompositionDelta<'_>,
) -> anyhow::Result<bool> {
    if repo.is_ancestor(commit, delta.base)? {
        return Ok(true);
    }
    for parent in delta.parents {
        if repo.is_ancestor(commit, parent)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `feat/gamma@a9a6c3e8ad93` when a local bookmark still holds the commit,
/// otherwise the commit alone.
fn recorded_member_name(commit: &CommitId, tips: &BookmarkTips) -> String {
    let named = tips.iter().find_map(|(reference, tip)| match reference {
        BookmarkRef::Local(branch) if tip == commit => Some(branch.to_string()),
        BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
    });
    named.map_or_else(
        || commit.short().to_owned(),
        |branch| format!("{branch}@{}", commit.short()),
    )
}

#[cfg(test)]
mod cut_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ledger::{Entry, Kind};
    use crate::release_model::last_recorded_cut;

    fn sample() -> Cut {
        Cut {
            name: "release/2026-07-30".to_owned(),
            parents: vec![CommitId::new("aaaa"), CommitId::new("bbbb")],
            provenance: vec![
                (CommitId::new("aaaa"), "pull/10/head".to_owned()),
                (CommitId::new("bbbb"), "feat/beta".to_owned()),
            ],
        }
    }

    #[test]
    fn the_message_records_where_every_parent_came_from() {
        // So a later sync knows what to check, including for a parent that was
        // never our branch.
        let message = sample().message();
        assert!(message.contains("release/2026-07-30"));
        assert!(message.contains("parent aaaa from pull/10/head"));
        assert!(message.contains("parent bbbb from feat/beta"));
    }

    #[test]
    fn a_recorded_branchs_workspace_with_nothing_left_is_identified() {
        let workspaces = vec![
            "default".to_owned(),
            "feat-alpha".to_owned(),
            "feat-beta".to_owned(),
        ];
        let recorded = vec!["feat/alpha".to_owned(), "feat/beta".to_owned()];
        let live = vec!["feat/beta".to_owned()];
        assert_eq!(
            workspaces_to_clean(&workspaces, &recorded, &live),
            vec!["feat-alpha".to_owned()]
        );
    }

    #[test]
    fn a_workspace_knives_never_recorded_or_something_holds_is_never_named() {
        // Naming the default workspace would delete the checkout the operator
        // stands in; naming a held or foreign one, another agent's work.
        let workspaces = vec![
            "default".to_owned(),
            "scratch".to_owned(),
            "release-2026-08-04".to_owned(),
        ];
        let recorded = vec!["default".to_owned(), "release/2026-08-04".to_owned()];
        let live = vec!["release/2026-08-04".to_owned()];
        assert_eq!(
            workspaces_to_clean(&workspaces, &recorded, &live),
            Vec::<String>::new()
        );
    }

    /// A cut event of `subject` recording `created` on `members`, as
    /// `record_cut_event` writes one: its text begins `cut <subject> as`,
    /// and its evidence is the commit, then the members.
    pub(super) fn cut_event(subject: &str, created: &str, members: &[&str]) -> Entry {
        let mut evidence = vec![created.to_owned()];
        evidence.extend(members.iter().map(|sha| (*sha).to_owned()));
        Entry {
            ts: "2026-08-15T00:00:00Z".to_owned(),
            owner: "an-agent".to_owned(),
            email: None,
            subject: Some(subject.to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: format!(
                "cut {subject} as {} with {} parent(s)",
                crate::ids::short_id(created),
                members.len()
            ),
            evidence,
            anchor: None,
            pr: None,
            parents: Vec::new(),
            claim_change: None,
            claim_change_why: None,
            previous_owner: None,
            previous_kind: None,
            previous_started: None,
            previous_last_seen: None,
            previous_why: None,
        }
    }

    #[test]
    fn the_newest_cut_event_is_the_recorded_composition() {
        // Given: two cut events; the newer one is the composition in hand.
        let entries = vec![
            cut_event("release/2026-08-14", "aaaa", &["1111", "2222", "3333"]),
            cut_event("release/2026-08-15", "bbbb", &["1111", "2222"]),
        ];
        // When/Then: the newest wins, and evidence splits into created + members.
        let recorded = last_recorded_cut(&entries, None).expect("a recorded cut");
        assert_eq!(recorded.commit, CommitId::new("bbbb"));
        assert_eq!(
            recorded.members,
            vec![CommitId::new("1111"), CommitId::new("2222")]
        );
    }

    #[test]
    fn a_note_or_foreign_prose_is_never_a_recorded_cut() {
        // Given: a note whose prose merely resembles a cut event, and an event
        // about something else entirely.
        let mut note = cut_event("release/2026-08-15", "aaaa", &["1111"]);
        note.kind = Kind::Note;
        let mut other = cut_event("feat/alpha", "bbbb", &["2222"]);
        other.text = "synced feat/alpha".to_owned();
        // When/Then: neither reads as a recorded composition.
        assert_eq!(last_recorded_cut(&[note, other], None), None);
    }

    #[test]
    fn a_cut_event_without_member_evidence_is_not_a_baseline() {
        // A cut always records created + members; anything thinner cannot say
        // what the previous composition was and must not pretend to.
        let thin = cut_event("release/2026-08-15", "aaaa", &[]);
        assert_eq!(last_recorded_cut(std::slice::from_ref(&thin), None), None);
        assert_eq!(last_recorded_cut(&[thin], None), None);
    }
}

/// Whether a cut kept at least as many tests as a single contributing branch.
///
/// A total below one branch's own count means the merge dropped that branch's
/// tests, which is silent: everything still compiles and the suite still passes,
/// just with less in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestCountCheck {
    NotConfigured,
    Kept { merged: u32, branch: u32 },
    Dropped { merged: u32, branch: u32 },
}

impl TestCountCheck {
    pub const fn compare(merged: u32, branch: u32) -> Self {
        if merged < branch {
            Self::Dropped { merged, branch }
        } else {
            Self::Kept { merged, branch }
        }
    }

    pub fn render(self) -> String {
        match self {
            Self::NotConfigured => {
                "  test count: not configured for this repo, so not checked".to_owned()
            }
            Self::Kept { merged, branch } => {
                format!("  test count: {merged} merged against {branch} on one branch, kept")
            }
            Self::Dropped { merged, branch } => format!(
                "  test count: {merged} merged is BELOW {branch} on a single branch, \
                 so the cut dropped that branch's tests"
            ),
        }
    }
}

/// Extract a count from a test command's output: the last number it prints.
pub fn parse_test_count(output: &str) -> Option<u32> {
    output
        .split(|c: char| !c.is_ascii_digit())
        .rfind(|piece| !piece.is_empty())
        .and_then(|piece| piece.parse().ok())
}

#[cfg(test)]
mod test_count_tests {
    use super::*;

    #[test]
    fn a_merged_total_below_one_branch_is_a_dropped_suite() {
        // Silent failure: everything compiles, the suite passes, there is just
        // less of it.
        assert_eq!(
            TestCountCheck::compare(40, 55),
            TestCountCheck::Dropped {
                merged: 40,
                branch: 55
            }
        );
        assert!(TestCountCheck::compare(40, 55).render().contains("BELOW"));
    }

    #[test]
    fn an_equal_or_greater_total_is_kept() {
        assert!(matches!(
            TestCountCheck::compare(55, 55),
            TestCountCheck::Kept { .. }
        ));
        assert!(matches!(
            TestCountCheck::compare(90, 55),
            TestCountCheck::Kept { .. }
        ));
    }

    #[test]
    fn an_unconfigured_check_never_renders_as_passed() {
        // Not looking must not read as looking and finding nothing wrong.
        let text = TestCountCheck::NotConfigured.render();
        assert!(text.contains("not checked"));
        assert!(!text.contains("kept"));
    }

    #[test]
    fn a_count_is_read_from_the_last_number_a_runner_prints() {
        assert_eq!(
            parse_test_count("test result: ok. 115 passed; 0 failed"),
            Some(0)
        );
        assert_eq!(parse_test_count("Ran 19 tests"), Some(19));
        assert_eq!(parse_test_count("no numbers here"), None);
    }
}

/// Did the cut keep at least as many tests as one contributing branch?
///
/// Runs the repo's configured test command at the cut and at one parent, in
/// throwaway workspaces. Absent configuration reports "not checked", never
/// "passed": counting tests has no portable form, so the command is per repo.
pub fn check_test_count(fork: &Fork<'_>, cut: &CommitId, parent: &CommitId) -> TestCountCheck {
    let Some(command) = fork.entry.test_count_command.as_deref() else {
        return TestCountCheck::NotConfigured;
    };
    let root = fork.workspace_root();
    let path = &fork.checkout.path;
    let merged = crate::jj::output_at_revision(path, root, cut.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    let single = crate::jj::output_at_revision(path, root, parent.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    match (merged, single) {
        (Some(merged), Some(single)) => TestCountCheck::compare(merged, single),
        _ => TestCountCheck::NotConfigured,
    }
}

/// What a release write resolved itself, what it left conflicted, and what to
/// do about the rest.
///
/// knives writes one resolution itself: the union of `CHANGELOG.md` and
/// `RELEASE_NOTES.md` where every side only inserted its own block
/// ([`crate::changelog_union`]). That resolution is the same every time, so a
/// person writing it by hand adds nothing. Each unioned file is named with the
/// first line of every block, in the order written: the union is verbatim and
/// never renumbers, so two blocks claiming one version read as two such lines,
/// and settling that is a person's.
///
/// Every other conflict stays a person's. Independent branches that each
/// append a config key land in the same regions, so a real cut carries real
/// conflicts: one ten-parent cut had a four-sided conflict in one file and a
/// three-sided one in another. Resolving those correctly is a semantic
/// judgement about the config, which a tool cannot make. Saying exactly where
/// they are, and what shape the resolution takes, is the part a tool can do.
/// An edit reports the same way: a duplicate carries the old resolutions
/// forward, so what is left is the conflict the edit itself introduced. A
/// resolution by hand is a change knives did not write, and every write over
/// the release refuses until `knives release adopt <release>` records it, so
/// the guidance names that step for `release`.
pub fn conflict_guidance(release: &str, files: &[String], unions: &[jj::UnionedFile]) -> String {
    use crate::ledger::inline_human_text;
    let mut lines = Vec::new();
    if !unions.is_empty() {
        lines.push(format!(
            "  {} release log(s) written as the union of their sides, every block verbatim \
             and in this order:",
            unions.len()
        ));
        // A path read from the tree, and each block's first line as a
        // member's file holds it.
        for union in unions {
            lines.push(format!("    {}", inline_human_text(&union.path)));
            lines.extend(
                union
                    .first_lines
                    .iter()
                    .map(|line| format!("      {}", inline_human_text(line))),
            );
        }
        lines.push(
            "  knives never renumbers: blocks that claim one version stay as each wrote it; \
             renumber by hand and record it with `knives release adopt --why`"
                .to_owned(),
        );
    }
    if files.is_empty() {
        lines.push(if unions.is_empty() {
            "  no conflicts in this release".to_owned()
        } else {
            "  no other conflicts in this release".to_owned()
        });
        return lines.join("\n");
    }
    lines.push(format!(
        "  {} conflicted file(s) left for a person:",
        files.len()
    ));
    // The paths are jj's, named by the conflict it reports.
    lines.extend(
        files
            .iter()
            .map(|file| format!("    {}", inline_human_text(file))),
    );
    lines.push(
        "  resolve as a union: keep every branch's addition, keep any shared helper \
         defined exactly once, and land each config key in every loader"
            .to_owned(),
    );
    lines.push("  then re-check the parent count and the test count before publishing".to_owned());
    // The release is a name read from the repository.
    lines.push(format!(
        "  after resolving them by hand, run `knives release adopt {}` before the next write, \
         which refuses until then",
        inline_human_text(release)
    ));
    lines.join("\n")
}

#[cfg(test)]
mod consumer_scope_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    #[test]
    fn a_siblings_pin_does_not_answer_this_repos_question() {
        // These forks cut releases on one dated scheme, so the same release name exists
        // in several of them. Unscoped, a sibling's pin was attributed to this repo,
        // which reads as "pinned at the newest cut" when this repo is not pinned at all.
        let _environment = crate::config::test_support::environment_lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("uv.lock"),
            "source = { git = \"https://forge.invalid/org/sandbox-runner.git?rev=release/2026-07-20\" }\n\
             source = { git = \"https://forge.invalid/org/sandbox-tools.git?rev=release/2026-07-28.2\" }\n",
        )
        .unwrap();

        let ours = scan_consumer_for(dir.path(), Some("sandbox-runner"), &ReleaseScheme::Dated);
        assert_eq!(ours.pins.len(), 1, "only our own pin: {:?}", ours.pins);
        assert_eq!(ours.pins[0].reference, "release/2026-07-20");
        assert_eq!(
            ours.notes,
            vec![format!(
                "{}: not a repository; pins read from the working copy",
                dir.path().display()
            )]
        );
        assert_eq!(ours.problems, Vec::<String>::new());

        let unscoped = scan_consumer_for(dir.path(), None, &ReleaseScheme::Dated);
        assert_eq!(unscoped.pins.len(), 2, "without a slug, every pin is kept");
    }

    #[test]
    fn consumer_notes_render_with_the_alert_prefix() {
        let plan = Plan {
            notes: vec!["consumer checkout is 1 commit behind its origin/main".to_owned()],
            ..Plan::default()
        };

        assert_eq!(
            render(&plan),
            "! consumer checkout is 1 commit behind its origin/main"
        );
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    fn a_clean_cut_says_so_rather_than_staying_silent() {
        assert!(conflict_guidance("release/2026-08-30", &[], &[]).contains("no conflicts"));
    }

    #[test]
    fn a_conflict_left_for_a_person_names_the_adopt_that_records_its_resolution() {
        // Given: a release write that left one file conflicted.
        // When: its guidance is rendered.
        let rendered = conflict_guidance("release/2026-08-30", &["shared.txt".to_owned()], &[]);
        // Then: it says to adopt the release by name once the file is
        // resolved by hand, before the next write.
        assert!(
            rendered.contains(
                "  after resolving them by hand, run `knives release adopt release/2026-08-30` \
                 before the next write, which refuses until then"
            ),
            "{rendered}"
        );
    }
}

#[cfg(test)]
mod members_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::cut_tests::cut_event;
    use super::*;
    use crate::config::test_support::run_jj;

    fn row(commit: &str, held_by: &[&str], advanced: &[&str], base_parent: bool) -> MemberRow {
        MemberRow {
            commit: CommitId::new(commit),
            held_by: held_by.iter().map(ToString::to_string).collect(),
            advanced: advanced.iter().map(ToString::to_string).collect(),
            base_parent,
            carrier: false,
            previous_release: None,
        }
    }

    fn report(audit: Option<CutAudit>) -> MembersReport {
        MembersReport {
            repo: "demo".to_owned(),
            release: "release/2026-08-30".to_owned(),
            commit: CommitId::new("rrrrrrrrrrrrrrrr"),
            parent_count: 4,
            members: vec![
                row("aaaaaaaaaaaaaaaa", &["feat/alpha"], &[], false),
                row(
                    "bbbbbbbbbbbbbbbb",
                    &[],
                    &["feat/beta advanced to cccccccccccc"],
                    false,
                ),
                row("dddddddddddddddd", &[], &[], false),
                row("eeeeeeeeeeeeeeee", &["main@upstream"], &[], true),
            ],
            audit,
            notes: Vec::new(),
            problems: Vec::new(),
        }
    }

    #[test]
    fn members_render_holders_advances_bare_bases_and_audit_buckets() {
        let audit = CutAudit {
            carried: vec!["feat/alpha@aaaaaaaaaaaa".to_owned()],
            missing: vec!["dddddddddddd".to_owned()],
            unexplained: vec!["Cargo.lock".to_owned()],
            inconclusive: Vec::new(),
        };

        let rendered = render_members(&report(Some(audit)));
        assert_eq!(
            rendered,
            concat!(
                "release/2026-08-30 @ rrrrrrrrrrrr — 4 parents\n",
                "- aaaaaaaaaaaa feat/alpha\n",
                "- bbbbbbbbbbbb feat/beta advanced to cccccccccccc\n",
                "- dddddddddddd bare commit — nothing holds it\n",
                "- eeeeeeeeeeee main@upstream (base parent)\n",
                "  feat/alpha@aaaaaaaaaaaa: the ledger records no write of the release in hand, so nothing measured its divergence from this member; check that divergence by hand\n",
                "  feat/beta@bbbbbbbbbbbb: carried (replay empty)\n",
                "  !! dddddddddddd: the cut tree is missing or diverges from the member's content\n",
                "  !! Cargo.lock: changed between the previous release and this cut with no member or trunk explaining it"
            )
        );
        // And: nothing in it calls an unmeasured divergence a recorded
        // resolution carried forward.
        assert!(!rendered.contains("carried forward"), "{rendered}");
        assert!(!rendered.contains("recorded resolution"), "{rendered}");
    }

    #[test]
    fn a_carried_row_names_how_the_release_in_hand_stands() {
        // Given: a member whose divergence the release held against already
        // has.
        let member = "feat/alpha@aaaaaaaaaaaa";

        // When/Then: held against a recorded write, the row says the release
        // in hand is one; against an unchecked one, that it is unrecorded;
        // against a release the ledger records no write of, that nothing
        // measured the divergence, so a person checks it by hand.
        assert_eq!(
            carried_row(member, CarriedFrom::Recorded),
            "  feat/alpha@aaaaaaaaaaaa: diverges where the release in hand (a recorded write) \
             already does; carried forward"
        );
        assert_eq!(
            carried_row(member, CarriedFrom::Unchecked),
            "  feat/alpha@aaaaaaaaaaaa: diverges where the unrecorded in-hand release already \
             does; record it with `knives release adopt` and cut again"
        );
        let unledgered = carried_row(member, CarriedFrom::Unledgered);
        assert_eq!(
            unledgered,
            "  feat/alpha@aaaaaaaaaaaa: the ledger records no write of the release in hand, so \
             nothing measured its divergence from this member; check that divergence by hand"
        );
        // And: it has neither the carried rows' `diverges where` shape nor
        // their `carried forward`, says nothing is a recorded resolution, and
        // names no `adopt`, which refuses a release with no recorded write.
        assert!(!unledgered.contains("diverges where"), "{unledgered}");
        assert!(!unledgered.contains("carried forward"), "{unledgered}");
        assert!(!unledgered.contains("recorded resolution"), "{unledgered}");
        assert!(!unledgered.contains("adopt"), "{unledgered}");
    }

    #[test]
    fn members_report_serializes_the_jj_parent_count() {
        let serialized = serde_json::to_value(report(None)).expect("report serializes");

        assert_eq!(serialized["parent_count"], serde_json::json!(4));
        assert!(serialized.get("audit").is_none());
    }

    #[test]
    fn audit_labels_keep_two_parents_advanced_by_one_branch_distinct() {
        let report = MembersReport {
            repo: "demo".to_owned(),
            release: "release/2026-08-30".to_owned(),
            commit: CommitId::new("rrrrrrrrrrrrrrrr"),
            parent_count: 2,
            members: vec![
                row(
                    "aaaaaaaaaaaaaaaa",
                    &[],
                    &["feat/shared advanced to ssssssssssss"],
                    false,
                ),
                row(
                    "bbbbbbbbbbbbbbbb",
                    &[],
                    &["feat/shared advanced to ssssssssssss"],
                    false,
                ),
            ],
            audit: Some(CutAudit {
                missing: vec!["feat/shared@aaaaaaaaaaaa".to_owned()],
                unexplained: Vec::new(),
                inconclusive: Vec::new(),
                carried: Vec::new(),
            }),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        let rendered = render_members(&report);
        assert!(rendered.contains(
            "!! feat/shared@aaaaaaaaaaaa: the cut tree is missing or diverges from the member's content"
        ));
        assert!(rendered.contains("feat/shared@bbbbbbbbbbbb: carried (replay empty)"));
    }

    #[test]
    fn members_render_holders_and_advances_together() {
        let report = MembersReport {
            repo: "demo".to_owned(),
            release: "release/2026-08-30".to_owned(),
            commit: CommitId::new("rrrrrrrrrrrrrrrr"),
            parent_count: 1,
            members: vec![row(
                "aaaaaaaaaaaaaaaa",
                &["keep/alpha"],
                &["feat/alpha advanced to bbbbbbbbbbbb"],
                false,
            )],
            audit: None,
            notes: Vec::new(),
            problems: Vec::new(),
        };

        assert!(
            render_members(&report)
                .contains("- aaaaaaaaaaaa keep/alpha, feat/alpha advanced to bbbbbbbbbbbb")
        );
    }

    #[test]
    fn member_parent_count_ignores_parent_shaped_commit_message_prose() {
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test repository");
        let repository = directory.path().join("repo");
        let repository_text = repository.display().to_string();
        run_jj(directory.path(), &["git", "init", &repository_text]);

        run_jj(&repository, &["new", "-r", "root()", "-m", "member alpha"]);
        run_jj(
            &repository,
            &["bookmark", "create", "feat/alpha", "-r", "@"],
        );
        run_jj(&repository, &["new", "-r", "root()", "-m", "member beta"]);
        run_jj(&repository, &["bookmark", "create", "feat/beta", "-r", "@"]);
        run_jj(
            &repository,
            &[
                "new",
                "-r",
                "feat/alpha",
                "-r",
                "feat/beta",
                "-m",
                "release: release/2026-08-30\n\n^parent prose-one\n^parent prose-two\n^parent prose-three",
            ],
        );
        run_jj(
            &repository,
            &["bookmark", "create", "release/2026-08-30", "-r", "@"],
        );

        let entry = RepoEntry::new("upstream", "origin");
        let fork = Fork::at("demo", &entry, &repository);
        let opened = Repo::open(&repository).expect("open test repository");
        let members = gather_members(&opened, &fork, "release/2026-08-30", (&[], false))
            .expect("gather members");

        assert_eq!(members.parent_count, 2);
    }

    #[test]
    fn a_release_bookmark_does_not_make_a_bare_parent_advanced() {
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test repository");
        let repository = directory.path().join("repo");
        let repository_text = repository.display().to_string();
        run_jj(directory.path(), &["git", "init", &repository_text]);
        run_jj(&repository, &["new", "-r", "root()", "-m", "bare parent"]);
        run_jj(&repository, &["new", "-r", "@", "-m", "release"]);
        run_jj(
            &repository,
            &["bookmark", "create", "release/2026-08-30", "-r", "@"],
        );

        let entry = RepoEntry::new("upstream", "origin");
        let fork = Fork::at("demo", &entry, &repository);
        let opened = Repo::open(&repository).expect("open test repository");
        let members = gather_members(&opened, &fork, "release/2026-08-30", (&[], false))
            .expect("gather members");

        assert_eq!(members.members.len(), 1);
        assert_eq!(members.members[0].held_by, Vec::<String>::new());
        assert_eq!(members.members[0].advanced, Vec::<String>::new());
        assert!(render_members(&members).contains("bare commit — nothing holds it"));
    }

    #[test]
    fn a_release_in_hand_is_recorded_by_its_tree_unrecorded_unmatched_or_has_no_record() {
        // Given: a release commit knives recorded on one member, the same
        // commit reworded (a metadata rewrite), a new commit on that member
        // holding exactly the recorded tree, and then the release's tree
        // edited by hand, which keeps the change id and moves the bookmark.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test repository");
        let repository = directory.path().join("repo");
        let repository_text = repository.display().to_string();
        let name = "release/2026-08-30";
        let at = |repository: &Path, revision: &str| {
            Repo::open(repository)
                .expect("open test repository")
                .resolve_commit(revision)
                .expect("resolve a fixture commit")
        };
        run_jj(directory.path(), &["git", "init", &repository_text]);
        run_jj(&repository, &["new", "-r", "root()", "-m", "member"]);
        std::fs::write(repository.join("member.txt"), "member\n").expect("write the member");
        run_jj(
            &repository,
            &["bookmark", "create", "feat/member", "-r", "@"],
        );
        run_jj(&repository, &["new", "-r", "feat/member", "-m", "release"]);
        run_jj(&repository, &["bookmark", "create", name, "-r", "@"]);
        let (member, written) = (at(&repository, "feat/member"), at(&repository, name));
        run_jj(
            &repository,
            &["new", "-r", "feat/member", "-m", "the same tree"],
        );
        run_jj(&repository, &["bookmark", "create", "same-tree", "-r", "@"]);
        let same_tree = at(&repository, "same-tree");
        run_jj(
            &repository,
            &["describe", "-r", name, "-m", "release, reworded"],
        );
        let reworded = at(&repository, name);
        run_jj(&repository, &["edit", name]);
        std::fs::write(repository.join("resolved.txt"), "by hand\n").expect("edit by hand");
        run_jj(&repository, &["new"]);
        let edited = at(&repository, name);
        let opened = Repo::open(&repository).expect("open test repository");
        let entry = RepoEntry::new("upstream", "origin");
        let ledger = [cut_event(name, written.as_str(), &[member.as_str()])];
        let standing = |ledger: &[Entry], commit: &CommitId| {
            let writes = EarlierWrites::read(&opened, name, ledger, None).expect("read writes");
            in_hand_write(
                &InHand {
                    repo: &opened,
                    ledger,
                    name,
                    commit,
                    writes: &writes,
                    published: None,
                },
                &entry,
            )
            .expect("read the standing")
        };
        let recorded_cut = |commit: &CommitId| RecordedWrite {
            kind: crate::release_model::RecordedWriteKind::Cut,
            commit: commit.clone(),
        };

        // When/Then: the recorded write, its reworded copy, and a commit
        // holding its tree on its member are recorded.
        assert_eq!(standing(&ledger, &written), InHandWrite::Recorded);
        assert_eq!(standing(&ledger, &reworded), InHandWrite::Recorded);
        assert_eq!(standing(&ledger, &same_tree), InHandWrite::Recorded);
        // And: the hand edit is unrecorded, measured against the write.
        assert_eq!(
            standing(&ledger, &edited),
            InHandWrite::Unrecorded {
                write: recorded_cut(&written)
            }
        );
        // And: a matched write this repository does not hold leaves the edit
        // unchecked, the write absent.
        let absent = CommitId::new("f".repeat(40));
        let elsewhere = [cut_event(name, absent.as_str(), &[member.as_str()])];
        assert_eq!(
            standing(&elsewhere, &edited),
            InHandWrite::Unchecked(Unchecked::WriteAbsent(recorded_cut(&absent)))
        );
        // And: a write recorded on other members matches nothing, which is
        // unchecked too.
        let other = [cut_event(name, absent.as_str(), &[absent.as_str()])];
        assert_eq!(
            standing(&other, &edited),
            InHandWrite::Unchecked(Unchecked::NoMatch(NoMatch::Members))
        );
        // And: a name the ledger records no write of has no record.
        assert_eq!(standing(&[], &edited), InHandWrite::NoRecord);
    }
}
