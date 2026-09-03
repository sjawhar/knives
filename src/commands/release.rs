//! `knives release`: plan, cut, edit or repair a release.
//!
//! Everything here is a check, never a prompt. A CLI in a non-interactive agent
//! session has nobody to ask, so it decides from evidence and says what it
//! decided. Planning is the default because everything else here writes: a cut
//! names a composition, and `include`, `drop`, `advance` and `rebase` change
//! one. Every one of them writes locally only, and none of them pushes.
// allow: SIZE_OK: 2395 lines - the release lifecycle's plan, members, cut, edit, audit, reap, and rebase operations are one domain seam.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::config::RepoEntry;
use crate::consumer_pins::{
    ConsumerHeadMemo, ConsumerPinSource, scan_consumer_for, scan_consumer_slug_with_heads,
};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, RebaseOutcome, ReleaseParent, Subject, stale_parents,
};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, ReleaseScheme, RepoName, is_our_release, is_release_name,
    strict_dated_release,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, RecordedParent};
use crate::pins::{Pin, PinKind};
use crate::release_model::{
    BranchSuccessions, MemberEvidence, MemberSuccession, RecordedCut, StackedHistoryContext,
    carried_from_tips, double_cut_findings, last_recorded_parents, member_parents, newest_release,
    release_refs_by_commit, repo_slug, stacked_history, trunk_positions,
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
/// A consumer frozen on an older release is not reached by an edit to this
/// one either way, and must not block it: judged over every pin, a fork whose
/// consumer sat frozen on one old cut refused to edit any release at all,
/// including a brand-new unpinned cut. A pin names the branch, so the release's
/// local and publish-remote views are the same release to it.
pub fn repair_effect(pins: &[Pin], release: &BranchName) -> RepairEffect {
    // Off-scheme pins consume the fork at a tag or branch of their own choosing;
    // they neither receive an in-place repair nor demand a new dated name.
    let mut of_release = pins
        .iter()
        .filter(|pin| pin.on_scheme && pin.reference == release.as_str())
        .peekable();
    if of_release.peek().is_none() {
        return RepairEffect::Unpinned;
    }
    if of_release.any(|pin| pin.kind == PinKind::Follows) {
        return RepairEffect::RepairInPlace;
    }
    RepairEffect::NewDatedName
}

pub fn cut_name(scheme: &ReleaseScheme, requested: Option<&str>) -> Result<String, String> {
    match (scheme, requested) {
        (ReleaseScheme::Dated, Some(name)) => Ok(name.to_owned()),
        (ReleaseScheme::Dated, None) => {
            Err("a dated release cut needs a name, e.g. release/2026-08-03".to_owned())
        }
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
}

#[derive(Debug, Default)]
pub struct Plan {
    pub repo: String,
    pub base_findings: Vec<Finding>,
    pub release: Option<String>,
    pub parents: Vec<(CommitId, Vec<String>)>,
    pub stale: Vec<Finding>,
    pub pins: Vec<Pin>,
    /// Informational: something worth saying that is not a failure.
    pub notes: Vec<String>,
    /// Could not answer. These, and only these, make the command exit non-zero
    /// for incompleteness. Keying on every note instead would make a routine
    /// remark like "14 superseded releases not scanned" look like a failure.
    pub problems: Vec<String>,
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
) -> anyhow::Result<Vec<CommitId>> {
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
    keep.push(repo.resolve_commit(&entry.upstream_trunk())?);
    Ok(keep)
}

/// Commits the recut would strand: reachable from the previous release or its
/// local descendants, and from no keeper.
///
/// Keepers are [`cut_keepers`]. The previous cut itself, parked working copies,
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

/// Commit identities currently named by our strict dated release references.
///
/// This shares the release-name trust boundary used by reap: local, `origin`,
/// and `release` refs count; names merely resembling a release on other
/// remotes do not. Callers subtract these ids rather than trusting descriptions.
pub fn our_dated_release_commits(tips: &BookmarkTips, publish_remote: &str) -> BTreeSet<CommitId> {
    tips.iter()
        .filter(|(reference, _)| {
            is_our_release(reference, &ReleaseScheme::Dated, publish_remote)
                && strict_dated_release(reference.branch().as_str()).is_some()
        })
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

/// Every ref of every superseded dated cut, on any remote that is ours.
///
/// Superseded means "not the newest dated name voted by a local, `origin`, or
/// `release` ref". The broader output excludes `upstream` and jj's `git`
/// tracking view, but keeps historical refs on every other remote for cleanup.
/// [`crate::ids::strict_dated_release`] keeps upstream-style semver names out
/// even on our remotes.
pub fn superseded_dated_releases(
    tips: &BookmarkTips,
    publish_remote: &str,
) -> Vec<(BookmarkRef, CommitId)> {
    let newest = tips
        .keys()
        // The vote is an allowlist, so someone else's cut cannot reap ours;
        // output remains broader to clean up our historical odd-remote refs.
        .filter(|reference| is_our_release(reference, &ReleaseScheme::Dated, publish_remote))
        .filter_map(|reference| strict_dated_release(reference.branch().as_str()))
        .max();
    let Some(newest) = newest else {
        return Vec::new();
    };
    let mut found: Vec<(BookmarkRef, CommitId)> = tips
        .iter()
        .filter(|(reference, _)| {
            ours_to_reap(reference)
                && strict_dated_release(reference.branch().as_str())
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
    /// outside the enumeration — a tag, an untracked remote bookmark — so jj
    /// would not abandon it. The expected outcome for a tagged release, not a
    /// failure: the name is gone and the commit stays reachable by its pin.
    /// Nothing else lands here; an abandon that failed for another reason is a
    /// note.
    pub forgotten_only: Vec<(String, String)>,
    /// (name, reason) pairs that were deliberately left alone.
    pub kept: Vec<(String, String)>,
    /// Non-fatal notes: an abandon that failed for a reason other than a pin.
    pub notes: Vec<String>,
}

/// Reap every superseded dated cut: forget its refs everywhere, abandon its commits.
///
/// The newest dated name never appears in the enumeration, and the
/// `previous_position` seam is `Fixed`-scheme-only while dated names are the
/// only thing enumerated, so neither needs a runtime gate here. Two things do:
/// a cut with local descendants is someone's stacked work (#4's third loss
/// mode) and is refused with the descendants named; and while the live cut
/// still carries conflicts, every superseded cut is kept, because the previous
/// cut is the only record of how those conflicts were last resolved — reaping
/// it while the successor is unsettled destroys the record exactly when an
/// abandon-and-recut would need it.
///
/// Parked workspace working copies — empty, undescribed — do not block: they sit
/// on release merges as a matter of course and jj rebases them harmlessly.
///
/// Never touches a remote. A later fetch re-materializes forgotten refs as
/// untracked (jj keeps no memory of forgetting); that is expected, harmless to
/// the default log, and cleared by the next reap. Correctness never depends on
/// reaping having run: the divergence detector ignores these refs regardless.
pub fn reap_superseded(
    repo_path: &Path,
    repo: &Repo,
    publish_remote: &str,
) -> anyhow::Result<ReapReport> {
    let tips = repo.bookmark_tips()?;
    let mut by_name = std::collections::BTreeMap::<String, Vec<CommitId>>::default();
    for (reference, commit) in superseded_dated_releases(&tips, publish_remote) {
        let targets = by_name.entry(reference.branch().to_string()).or_default();
        if !targets.contains(&commit) {
            targets.push(commit);
        }
    }
    if !by_name.is_empty()
        && let Some((live, commit)) = live_dated_release(&tips, publish_remote)
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
            notes: Vec::new(),
        });
    }

    let mut report = ReapReport {
        reaped: Vec::new(),
        forgotten_only: Vec::new(),
        kept: Vec::new(),
        notes: Vec::new(),
    };
    let mut entries: Vec<(String, Vec<CommitId>)> = Vec::new();
    'names: for (name, targets) in by_name {
        for target in &targets {
            let descendants = crate::jj::commits_matching(
                repo_path,
                &format!(
                    "(descendants({id}) ~ {id}) ~ (empty() & description(exact:\"\"))",
                    id = target.as_str()
                ),
            )?;
            if !descendants.is_empty() {
                let sample: Vec<&str> = descendants.iter().take(3).map(CommitId::short).collect();
                report.kept.push((
                    name.clone(),
                    format!("has local descendant(s): {}", sample.join(", ")),
                ));
                continue 'names;
            }
        }
        entries.push((name, targets));
    }
    if !entries.is_empty() {
        let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
        let operation = format!("knives: reap {}", names.join(", "));
        let outcome = crate::jj::forget_and_abandon(repo_path, &entries, &operation)?;
        report.reaped = outcome.abandoned;
        for (name, error) in outcome.refused {
            match error {
                crate::jj::JjError::Immutable { commit, pin } => report
                    .forgotten_only
                    .push((name, format!("{commit} still pinned by {pin}"))),
                other => report.notes.push(format!(
                    "{name}: refs forgotten, commit not abandoned: {other}"
                )),
            }
        }
    }
    Ok(report)
}

/// The newest dated cut's branch name — unqualified, unlike
/// [`previous_release_for_cut`]'s — and its live commit: the local ref when
/// present, otherwise whichever of our remotes carries it.
fn live_dated_release(tips: &BookmarkTips, publish_remote: &str) -> Option<(BranchName, CommitId)> {
    let newest = tips
        .keys()
        .filter(|reference| is_our_release(reference, &ReleaseScheme::Dated, publish_remote))
        .filter_map(|reference| strict_dated_release(reference.branch().as_str()))
        .max()?;
    tips.iter()
        .filter(|(reference, _)| {
            is_our_release(reference, &ReleaseScheme::Dated, publish_remote)
                && strict_dated_release(reference.branch().as_str()).as_ref() == Some(&newest)
        })
        .max_by_key(|(reference, _)| u8::from(reference.is_local()))
        .map(|(reference, commit)| (reference.branch().clone(), commit.clone()))
}

/// The release in hand and what it does not carry.
///
/// `ledger` is this repository's ledger, read by the caller: the last recorded
/// parent set is what names a member whose branch was rebased outside jj or
/// landed upstream, where the repository no longer can.
pub fn plan(
    name: &RepoName,
    entry: &RepoEntry,
    consumers: &ConsumerInputs<'_>,
    ledger: &[Entry],
) -> anyhow::Result<Plan> {
    let mut plan = Plan {
        repo: name.to_string(),
        ..Plan::default()
    };
    let repo = Repo::open(&entry.path)?;
    let tips = repo.bookmark_tips()?;

    // The newest release we cut. Historical ones are frozen and not our concern.
    let scheme = entry.release_scheme();
    let (findings, notes) =
        double_cut_findings(&entry.path, &tips, &scheme, entry.publish_remote())?;
    plan.base_findings.extend(findings);
    plan.notes.extend(notes);
    let publish_remote = entry.publish_remote();
    let newest = newest_release(&tips, &scheme, publish_remote);

    let Some((reference, commit)) = newest else {
        plan.notes.push(match scheme {
            ReleaseScheme::Dated => {
                "no dated release found; the first cut has nothing to repair".to_owned()
            }
            ReleaseScheme::Fixed(fixed) => {
                format!("fixed release branch {fixed} has no cut yet; the first cut has nothing to repair")
            }
        });
        return Ok(plan);
    };
    plan.release = Some(reference.to_string());

    let parents = repo.parents_of(commit.as_str())?;
    let trunk_tip = repo.resolve_commit(&entry.upstream_trunk()).ok();
    let trunks = trunk_positions(&repo, entry)?;
    let base = match &trunk_tip {
        Some(trunk) => shared_base(&repo, &commit, trunk)?,
        None => None,
    };
    let mut member_parents = Vec::new();
    for parent in &parents {
        let trunk_reachable = match &trunk_tip {
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
    let releases = release_refs_by_commit(&tips, &scheme, publish_remote);
    let stacked_context = (!trunks.is_empty()).then_some(StackedHistoryContext {
        repo: &repo,
        trunks: &trunks,
        releases: &releases,
    });
    if let Some(context) = stacked_context {
        for parent in &member_parents {
            if let Some(finding) = stacked_history(context, &parent_label(parent), &parent.commit)?
            {
                plan.base_findings.push(finding);
            }
        }
    }
    plan.stale = stale_parents(&member_parents, &tips);
    plan.notes.extend(local_branch_notes(&LocalBranchNotes {
        repo: &repo,
        reference: &reference.to_string(),
        release: &commit,
        parents: &parents,
        trunks: &trunks,
        stacked: stacked_context,
        branches: &carried_from_tips(&tips, entry.trunk(), &scheme),
        recorded: last_recorded_parents(ledger, &reference.to_string()),
    })?);
    plan.parents = parents
        .into_iter()
        .map(|parent| {
            let names = parent.bookmarks.iter().map(ToString::to_string).collect();
            (parent.commit, names)
        })
        .collect();

    add_consumer_pins(&mut plan, entry, consumers, &scheme);
    Ok(plan)
}

fn add_consumer_pins(
    plan: &mut Plan,
    entry: &RepoEntry,
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
    let slug = repo_slug(entry);
    for consumer in consumers.slugs {
        let scan = scan_consumer_slug_with_heads(
            consumers.forge,
            consumers.cache_root,
            &entry.path,
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
    branches: &'a [(String, CommitId)],
    /// The release's last recorded parent set, for a branch the repository can
    /// no longer tie to its parent.
    recorded: &'a [RecordedParent],
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
        // ancestry alone reads as "advanced". It is neither advanced nor
        // includable as it stands: both verbs refuse it, because carrying it would
        // put the cut in its own successor's ancestry. The same goes for a branch
        // whose history carries any release merge.
        if repo.is_ancestor(release, tip)? {
            notes.push(format!(
                "{branch} is stacked on {reference} rather than the trunk; rebase it off the \
                 trunk before including it"
            ));
            continue;
        }
        if let Some(context) = stacked
            && let Some(stacked) = stacked_history(context, branch, tip)?
        {
            notes.push(format!(
                "{}; rebase it off the trunk before including it",
                stacked.detail
            ));
            continue;
        }
        let succession = MemberSuccession::of(repo, trunks, tip)?;
        let lookup = member_parents(&succession, &parents, recorded, branch)?;
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

/// Gather one parent's current holders, descendants, and trunk relation.
fn member_row(
    opened: &Repo,
    parent: ReleaseParent,
    trunk: Option<&CommitId>,
    successions: &BranchSuccessions<'_>,
) -> anyhow::Result<MemberRow> {
    let advanced = successions
        .successors_of(&parent.commit)?
        .into_iter()
        .map(|(branch, tip)| format!("{branch} advanced to {}", tip.short()))
        .collect();
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
    })
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
pub fn gather_members(
    opened: &Repo,
    entry: &RepoEntry,
    name: &str,
    verify: bool,
) -> anyhow::Result<MembersReport> {
    let commit = opened.resolve_commit(name)?;
    let parents = opened.parents_of(commit.as_str())?;
    let trunk_name = entry.upstream_trunk();
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
    let branches = carried_from_tips(
        &opened.bookmark_tips()?,
        entry.trunk(),
        &entry.release_scheme(),
    );
    let successions = BranchSuccessions::of(opened, &trunks, &branches)?;
    let members: Vec<MemberRow> = parents
        .into_iter()
        .map(|parent| member_row(opened, parent, trunk.as_ref(), &successions))
        .collect::<anyhow::Result<_>>()?;
    let audit = if verify {
        trunk
            .as_ref()
            .map(|trunk| {
                let members: Vec<(String, CommitId)> = members
                    .iter()
                    .filter(|member| !member.base_parent)
                    .map(|member| (member_label(member), member.commit.clone()))
                    .collect();
                audit_cut(
                    &entry.path,
                    &members,
                    CutSubject::Committed(&commit),
                    AuditContext {
                        previous: None,
                        trunk,
                    },
                )
            })
            .transpose()?
    } else {
        None
    };
    Ok(MembersReport {
        repo: entry.path.display().to_string(),
        release: name.to_owned(),
        commit,
        parent_count: members.len(),
        members,
        audit,
        notes: Vec::new(),
        problems,
    })
}

/// Render the gathered parent state and optional audit in the cut command's words.
pub fn render_members(report: &MembersReport) -> String {
    let mut lines = vec![format!(
        "{} @ {} — {} parents",
        report.release,
        report.commit.short(),
        report.parent_count
    )];
    for member in &report.members {
        let mut description = member.held_by.join(", ");
        if !member.advanced.is_empty() {
            if !description.is_empty() {
                description.push_str(", ");
            }
            description.push_str(&member.advanced.join(", "));
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
            lines.push(format!(
                "  {name}: diverges where the previous release already did \
                 (a recorded resolution); carried forward"
            ));
        }
        for name in &audit.inconclusive {
            lines.push(format!(
                "  {name}: content check inconclusive (replay conflicted; \
                 re-check after resolving the cut's conflicts)"
            ));
        }
        for member in report.members.iter().filter(|member| !member.base_parent) {
            let name = member_label(member);
            if !audit.carried.contains(&name)
                && !audit.inconclusive.contains(&name)
                && !audit.missing.contains(&name)
            {
                lines.push(format!("  {name}: carried (replay empty)"));
            }
        }
        for name in &audit.missing {
            lines.push(format!(
                "  !! {name}: the cut tree is missing or diverges from the member's content"
            ));
        }
        for file in &audit.unexplained {
            lines.push(format!(
                "  !! {file}: changed between the previous release and this cut \
                 with no member or trunk explaining it"
            ));
        }
    }
    lines.extend(report.notes.iter().map(|note| format!("! {note}")));
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("!! {problem}")),
    );
    lines.join("\n")
}

pub fn render(plan: &Plan) -> String {
    let mut lines: Vec<String> = plan
        .problems
        .iter()
        .map(|problem| format!("!! {problem}"))
        .chain(plan.notes.iter().map(|note| format!("! {note}")))
        .collect();
    let Some(release) = &plan.release else {
        return lines.join("\n");
    };

    lines.push(format!("{}: {release}", plan.repo));
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
    for (commit, names) in &plan.parents {
        let held = if names.is_empty() {
            "no bookmark".to_owned()
        } else {
            names.join(", ")
        };
        lines.push(format!("    {}  {held}", commit.short()));
    }

    if plan.stale.is_empty() {
        lines.push("  every parent is still its branch tip".to_owned());
    } else {
        lines.push(format!("  {} stale parent(s):", plan.stale.len()));
        for finding in &plan.stale {
            lines.push(format!("    {}", finding.detail));
        }
    }
    for finding in &plan.base_findings {
        lines.push(format!("  !! {}", finding.detail));
    }

    // The verdict line below says "nothing pins this release" on its own; a
    // heading over an empty list added a line about a consumer that may not exist.
    if !plan.pins.is_empty() {
        lines.push("  pinned by:".to_owned());
        lines.push(crate::pins::render(&plan.pins));
    }
    // A consumer that could not be consulted may hold the pin the verdict would
    // deny; the census refuses the same no-pin claim after a failed scan.
    lines.push(if plan.problems.is_empty() {
        match repair_effect(&plan.pins, BookmarkRef::parse(release).branch()) {
            RepairEffect::RepairInPlace => {
                "  at least one consumer follows the branch: repair in place, no new dated name"
                    .to_owned()
            }
            RepairEffect::NewDatedName => {
                "  every pin of this release is frozen: editing it reaches nobody; the next cut \
             needs a new dated suffix"
                    .to_owned()
            }
            RepairEffect::Unpinned => "  nothing pins this release: either is safe".to_owned(),
        }
    } else {
        "  pinned-ness unknown: a consumer could not be consulted (see above)".to_owned()
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

        let superseded = superseded_dated_releases(&tips, "release");
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
        let names: Vec<String> = superseded_dated_releases(&tips, "release")
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
        let names: Vec<String> = superseded_dated_releases(&tips, "release")
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
        let names: Vec<String> = superseded_dated_releases(&tips, "release")
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
        let names: Vec<String> = superseded_dated_releases(&tips, "release")
            .iter()
            .map(|(r, _)| r.to_string())
            .collect();
        assert_eq!(names, vec!["release/2026-08-04".to_owned()]);
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
            repair_effect(&pins, &BranchName::new("release/2026-07-28")),
            RepairEffect::RepairInPlace
        );
    }

    #[test]
    fn all_frozen_means_a_new_dated_name() {
        assert_eq!(
            repair_effect(
                &[pin(PinKind::Frozen)],
                &BranchName::new("release/2026-07-28")
            ),
            RepairEffect::NewDatedName
        );
    }

    #[test]
    fn nothing_pinning_it_leaves_the_choice_open() {
        assert_eq!(
            repair_effect(&[], &BranchName::new("release/2026-07-28")),
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
            repair_effect(&pins, &BranchName::new("release/2026-08-31")),
            RepairEffect::Unpinned
        );
        assert_eq!(
            repair_effect(
                &pins,
                BookmarkRef::parse("release/2026-07-28@release").branch()
            ),
            RepairEffect::NewDatedName,
            "the publish remote's view of the same release is the same release"
        );
    }

    #[test]
    fn an_off_scheme_pin_alone_leaves_the_repair_choice_open() {
        // A consumer pinned at its own tag is not consuming releases: repairing in
        // place cannot reach it and a new dated name would not either.
        let mut off_scheme = pin(PinKind::Frozen);
        off_scheme.on_scheme = false;
        off_scheme.reference = "acme-pin-0.4.47.dev7".to_owned();
        assert_eq!(
            repair_effect(&[off_scheme], &BranchName::new("acme-pin-0.4.47.dev7")),
            RepairEffect::Unpinned
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
    let trunk_tip = repo.resolve_commit(&entry.upstream_trunk()).ok();
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
            entry.upstream_trunk()
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
    /// did: a recorded conflict resolution, published and deliberate. Reported,
    /// never refused — the audit charges a cut only with divergence it
    /// introduces.
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

/// Workspaces with no branch left among `branches`.
///
/// They are cheap to create, which is why they accumulate; nothing else reaps
/// them. What counts as "left" is the caller's list, so a cut can decide
/// whether a branch it did not carry still has a bookmark holding it.
pub fn workspaces_to_clean(workspaces: &[String], branches: &[String]) -> Vec<String> {
    let kept: Vec<String> = branches.iter().map(|b| b.replace('/', "-")).collect();
    workspaces
        .iter()
        .filter(|name| name.as_str() != "default" && !kept.contains(name))
        .cloned()
        .collect()
}

/// Build the candidate cut — in a scratch transaction the audit can read and
/// a failed audit simply drops — and verify it has exactly the parents asked
/// for.
///
/// `Some(previous)` duplicates the previous release onto the new parent set,
/// preserving its recorded conflict resolutions. Adding a branch or advancing
/// the base therefore avoids replaying old conflicts; dropping an entangled
/// branch surfaces a focused conflict rather than silently retaining its
/// content. `None` builds a fresh flat merge. Both paths produce a flat octopus
/// of exactly `request.parents`.
/// The parent-count guard catches a silent failure: a cut that dropped a parent
/// looks exactly like one that did not, until work goes missing downstream.
///
/// Nothing is published until [`publish_cut`], and nothing ever pushes.
pub fn candidate_cut(
    repo: &Path,
    request: &Cut,
    previous: Option<&CommitId>,
) -> anyhow::Result<jj::Candidate> {
    let candidate = jj::candidate_release(
        repo,
        jj::CutSpec {
            source: previous.cloned(),
            parents: request.parents.clone(),
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
/// name, which may already exist on the remote.
pub fn publish_cut(
    candidate: jj::Candidate,
    name: &str,
    scheme: &ReleaseScheme,
) -> anyhow::Result<CommitId> {
    let motion = match scheme {
        ReleaseScheme::Dated => jj::BookmarkMotion::ForwardOnly,
        ReleaseScheme::Fixed(_) => jj::BookmarkMotion::Anywhere,
    };
    Ok(candidate.publish((name, motion), &format!("knives: cut {name}"))?)
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
    mut subject: CutSubject<'_>,
    context: AuditContext<'_>,
) -> anyhow::Result<CutAudit> {
    let mut audit = CutAudit::default();
    let cut_is_conflicted = !subject.conflicted_files(repo)?.is_empty();
    for (name, tip) in members {
        match subject.replay_outcome(repo, context.trunk.as_str(), tip.as_str())? {
            RebaseOutcome::Empty => {}
            RebaseOutcome::Conflicted if cut_is_conflicted => audit.inconclusive.push(name.clone()),
            RebaseOutcome::CleanNonEmpty | RebaseOutcome::Conflicted => {
                // Divergence the previous release already carried is a recorded
                // resolution, not a loss: refusing it would make every release
                // with a hand-resolved conflict fail its own re-cut forever.
                let carried_before = match context.previous {
                    Some(previous) => !matches!(
                        crate::jj::probe_net_diff(
                            repo,
                            context.trunk.as_str(),
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
    if let Some(previous) = context.previous {
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
pub fn cut(repo: &Path, request: &Cut, scheme: &ReleaseScheme) -> anyhow::Result<CommitId> {
    let candidate = candidate_cut(repo, request, None)?;
    publish_cut(candidate, &request.name, scheme)
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
}

/// What the composition gate compares: the candidate's parents against the
/// members the previous cut's ledger event recorded.
#[derive(Debug)]
pub struct CompositionDelta<'a> {
    pub recorded: &'a RecordedCut,
    /// The candidate's parents, for the identity and ancestry fast paths.
    pub parents: &'a [CommitId],
    /// The audit's base: what every current member forks from. A recorded
    /// member that is an ancestor of it entered the candidate through the
    /// base — how a merge-landed member reads as carried after a rebase.
    pub base: &'a CommitId,
    /// The upstream trunk tip. A member reachable from it but not from the
    /// base or any parent landed upstream past what the candidate ships:
    /// the trunk carries it, this cut does not, and that is a drop.
    pub trunk: &'a CommitId,
    pub tips: &'a BookmarkTips,
}

/// Recorded members of the previous cut that the candidate does not carry.
///
/// Identity and ancestry account for a member that is still a parent, was
/// advanced past, or entered through the candidate's base; the content replay
/// accounts for one that landed upstream as a squash — the same measure
/// [`audit_cut`] applies to current members, taken from the member's own fork
/// point so a moved base is never charged to the member. A member the trunk
/// reaches but the candidate does not is dropped without a replay: its fork
/// point degenerates to the member itself, and that replay would read empty
/// without consulting the candidate at all. A recorded commit this repository
/// cannot resolve counts as dropped: unverifiable must not read as carried.
pub fn uncarried_recorded_members(
    repo: &Repo,
    candidate: &mut jj::Candidate,
    delta: &CompositionDelta<'_>,
) -> anyhow::Result<CompositionCheck> {
    let mut check = CompositionCheck::default();
    let candidate_conflicted = !candidate.conflicted_files()?.is_empty();
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
        let mut carried = repo.is_ancestor(member, delta.base)?;
        for parent in delta.parents {
            if carried {
                break;
            }
            carried = repo.is_ancestor(member, parent)?;
        }
        if carried {
            continue;
        }
        if repo.is_ancestor(member, delta.trunk)? {
            check.dropped.push(recorded_member_name(member, delta.tips));
            continue;
        }
        let base = repo
            .common_ancestor(std::slice::from_ref(member), delta.trunk)?
            .unwrap_or_else(|| delta.trunk.clone());
        match candidate.replay_outcome(base.as_str(), member.as_str())? {
            RebaseOutcome::Empty => {}
            RebaseOutcome::Conflicted if candidate_conflicted => {
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
    fn workspaces_for_dropped_branches_are_identified() {
        let workspaces = vec![
            "default".to_owned(),
            "feat-alpha".to_owned(),
            "feat-beta".to_owned(),
        ];
        let carried = vec!["feat/beta".to_owned()];
        assert_eq!(
            workspaces_to_clean(&workspaces, &carried),
            vec!["feat-alpha".to_owned()]
        );
    }

    #[test]
    fn the_default_workspace_is_never_reaped() {
        // Reaping it would delete the checkout the operator is standing in.
        let workspaces = vec!["default".to_owned()];
        assert!(workspaces_to_clean(&workspaces, &[]).is_empty());
    }

    fn cut_event(subject: &str, created: &str, members: &[&str]) -> Entry {
        let mut evidence = vec![created.to_owned()];
        evidence.extend(members.iter().map(|sha| (*sha).to_owned()));
        Entry {
            ts: "2026-08-15T00:00:00Z".to_owned(),
            owner: "an-agent".to_owned(),
            subject: Some(subject.to_owned()),
            kind: Kind::Event,
            disposition: None,
            text: format!(
                "cut {subject} as {created} with {} parent(s)",
                members.len()
            ),
            evidence,
            anchor: None,
            pr: None,
            parents: Vec::new(),
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
pub fn check_test_count(entry: &RepoEntry, cut: &CommitId, parent: &CommitId) -> TestCountCheck {
    let Some(command) = entry.test_count_command.as_deref() else {
        return TestCountCheck::NotConfigured;
    };
    let root = entry.workspace_root();
    let merged = crate::jj::output_at_revision(&entry.path, root, cut.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    let single = crate::jj::output_at_revision(&entry.path, root, parent.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    match (merged, single) {
        (Some(merged), Some(single)) => TestCountCheck::compare(merged, single),
        _ => TestCountCheck::NotConfigured,
    }
}

/// What a release's conflicts mean, and what to do about them.
///
/// Reported, never auto-resolved. Independent branches that each append a config
/// key land in the same regions, so a real cut carries real conflicts: one
/// ten-parent cut had a four-sided conflict in one file and a three-sided one in
/// another. Resolving those correctly is a semantic judgement about the config,
/// which a tool cannot make. Saying exactly where they are, and what shape the
/// resolution takes, is the part a tool can do. An edit reports the same way: a
/// duplicate carries the old resolutions forward, so what is left is the
/// conflict the edit itself introduced.
pub fn conflict_guidance(files: &[String]) -> String {
    if files.is_empty() {
        return "  no conflicts in this release".to_owned();
    }
    let mut lines = vec![format!(
        "  {} conflicted file(s), which is expected:",
        files.len()
    )];
    lines.extend(files.iter().map(|file| format!("    {file}")));
    lines.push(
        "  resolve as a union: keep every branch's addition, keep any shared helper \
         defined exactly once, and land each config key in every loader"
            .to_owned(),
    );
    lines.push("  then re-check the parent count and the test count before publishing".to_owned());
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
        assert!(ours.problems.is_empty());

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
        assert!(conflict_guidance(&[]).contains("no conflicts"));
    }

    #[test]
    fn conflicts_are_named_with_the_shape_of_their_resolution() {
        // Resolving them correctly is a semantic judgement about the config,
        // which a tool cannot make. Saying where they are, and that the shape is
        // a union with one shared helper and the key in every loader, is the
        // part a tool can do.
        let text = conflict_guidance(&["infra/lib/config.py".to_owned()]);
        assert!(text.contains("infra/lib/config.py"));
        assert!(text.contains("union"));
        assert!(text.contains("every loader"));
    }
}

#[cfg(test)]
mod members_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;

    fn row(commit: &str, held_by: &[&str], advanced: &[&str], base_parent: bool) -> MemberRow {
        MemberRow {
            commit: CommitId::new(commit),
            held_by: held_by.iter().map(ToString::to_string).collect(),
            advanced: advanced.iter().map(ToString::to_string).collect(),
            base_parent,
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

        assert_eq!(
            render_members(&report(Some(audit))),
            concat!(
                "release/2026-08-30 @ rrrrrrrrrrrr — 4 parents\n",
                "- aaaaaaaaaaaa feat/alpha\n",
                "- bbbbbbbbbbbb feat/beta advanced to cccccccccccc\n",
                "- dddddddddddd bare commit — nothing holds it\n",
                "- eeeeeeeeeeee main@upstream (base parent)\n",
                "  feat/alpha@aaaaaaaaaaaa: diverges where the previous release already did (a recorded resolution); carried forward\n",
                "  feat/beta@bbbbbbbbbbbb: carried (replay empty)\n",
                "  !! dddddddddddd: the cut tree is missing or diverges from the member's content\n",
                "  !! Cargo.lock: changed between the previous release and this cut with no member or trunk explaining it"
            )
        );
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

    fn run_jj(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("jj")
            .current_dir(path)
            .args(args)
            .output()
            .expect("run jj test fixture command");
        assert!(
            output.status.success(),
            "jj {} failed:\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn member_parent_count_ignores_parent_shaped_commit_message_prose() {
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test repository");
        let repository = directory.path().join("repo");
        let repository_text = repository.display().to_string();
        run_jj(directory.path(), &["git", "init", &repository_text]);
        run_jj(
            &repository,
            &["config", "set", "--repo", "user.name", "knives tests"],
        );
        run_jj(
            &repository,
            &[
                "config",
                "set",
                "--repo",
                "user.email",
                "tests@example.invalid",
            ],
        );

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

        let entry = RepoEntry {
            path: repository,
            upstream: "upstream".to_owned(),
            origin: "origin".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        };
        let opened = Repo::open(&entry.path).expect("open test repository");
        let members =
            gather_members(&opened, &entry, "release/2026-08-30", false).expect("gather members");

        assert_eq!(members.parent_count, 2);
    }

    #[test]
    fn a_release_bookmark_does_not_make_a_bare_parent_advanced() {
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test repository");
        let repository = directory.path().join("repo");
        let repository_text = repository.display().to_string();
        run_jj(directory.path(), &["git", "init", &repository_text]);
        run_jj(
            &repository,
            &["config", "set", "--repo", "user.name", "knives tests"],
        );
        run_jj(
            &repository,
            &[
                "config",
                "set",
                "--repo",
                "user.email",
                "tests@example.invalid",
            ],
        );
        run_jj(&repository, &["new", "-r", "root()", "-m", "bare parent"]);
        run_jj(&repository, &["new", "-r", "@", "-m", "release"]);
        run_jj(
            &repository,
            &["bookmark", "create", "release/2026-08-30", "-r", "@"],
        );

        let entry = RepoEntry {
            path: repository,
            upstream: "upstream".to_owned(),
            origin: "origin".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        };
        let opened = Repo::open(&entry.path).expect("open test repository");
        let members =
            gather_members(&opened, &entry, "release/2026-08-30", false).expect("gather members");

        assert_eq!(members.members.len(), 1);
        assert!(members.members[0].held_by.is_empty());
        assert!(members.members[0].advanced.is_empty());
        assert!(render_members(&members).contains("bare commit — nothing holds it"));
    }
}
