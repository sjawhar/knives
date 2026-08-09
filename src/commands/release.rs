//! `knives release`: plan, cut, edit or repair a release.
//!
//! Everything here is a check, never a prompt. A CLI in a non-interactive agent
//! session has nobody to ask, so it decides from evidence and says what it
//! decided. Planning is the default because everything else here writes: a cut
//! names a composition, and `include`, `drop`, `advance` and `rebase` change
//! one. Every one of them writes locally only, and none of them pushes.
// allow: SIZE_OK: 1539 lines - the release lifecycle's plan, cut, edit, audit, reap, and rebase operations are one domain seam.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::config::{RepoEntry, Role};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, RebaseOutcome, ReleaseParent, Subject, stale_parents,
};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, ReleaseScheme, RepoName, is_our_release, is_release_name,
    strict_dated_release,
};
use crate::jj::{self, OriginTrunk, Repo};
use crate::pins::{PIN_FILES, Pin, PinKind, scan};

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

pub fn repair_effect(pins: &[Pin]) -> RepairEffect {
    if pins.is_empty() {
        return RepairEffect::Unpinned;
    }
    if pins.iter().any(|pin| pin.kind == PinKind::Follows) {
        return RepairEffect::RepairInPlace;
    }
    RepairEffect::NewDatedName
}

/// Scan a consumer checkout for pins of one repo's releases.
///
/// Scoped by `slug`, the repository's name as it appears in a dependency line. These
/// forks cut releases on one dated scheme, so `release/2026-07-28` exists in several of
/// them at once; an unscoped scan attributed a sibling's pin to this repo, which reads
/// as "pinned at the newest cut" when it is not pinned here at all. `None` keeps every
/// pin, for a caller that genuinely wants the whole file.
pub fn scan_consumer_for(
    consumer: &Path,
    slug: Option<&str>,
    scheme: &ReleaseScheme,
) -> (Vec<Pin>, Vec<String>) {
    let mut pins = Vec::new();
    let mut notes = Vec::new();
    match jj::origin_trunk(consumer) {
        Ok(OriginTrunk::Reference(branch)) => {
            let mut checkout_lag = None;
            for name in PIN_FILES {
                match jj::file_at_ref(consumer, &branch, name) {
                    Ok(Some((text, behind))) => {
                        pins.extend(scanned_pins(name, &text, slug, scheme));
                        checkout_lag = checkout_lag.or_else(|| (behind > 0).then_some(behind));
                    }
                    Ok(None) => {}
                    Err(error) => notes.push(format!(
                        "{}: could not read {name} at {branch}: {error}",
                        consumer.display()
                    )),
                }
            }
            if let Some(behind) = checkout_lag {
                notes.push(format!(
                    "{} checkout is {behind} commit(s) behind its {branch}",
                    consumer.display()
                ));
            }
        }
        Ok(OriginTrunk::Missing) => {
            extend_working_copy_pins(&mut pins, consumer, slug, scheme);
            notes.push(format!(
                "{}: no origin trunk resolved; pins read from the working copy",
                consumer.display()
            ));
        }
        Ok(OriginTrunk::NotRepository) => {
            extend_working_copy_pins(&mut pins, consumer, slug, scheme);
            notes.push(format!(
                "{}: not a repository; pins read from the working copy",
                consumer.display()
            ));
        }
        Err(error) => {
            extend_working_copy_pins(&mut pins, consumer, slug, scheme);
            notes.push(format!(
                "{}: could not resolve its origin trunk ({error}); pins read from the working copy",
                consumer.display()
            ));
        }
    }
    (pins, notes)
}

fn extend_working_copy_pins(
    pins: &mut Vec<Pin>,
    consumer: &Path,
    slug: Option<&str>,
    scheme: &ReleaseScheme,
) {
    for name in PIN_FILES {
        let path = consumer.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            pins.extend(scanned_pins(name, &text, slug, scheme));
        }
    }
}

fn scanned_pins(file: &str, text: &str, slug: Option<&str>, scheme: &ReleaseScheme) -> Vec<Pin> {
    scan(file, text, scheme)
        .into_iter()
        .filter(|pin| slug.is_none_or(|slug| pin.source.contains(slug)))
        .collect()
}

/// The repository's name as it appears in a dependency line, e.g. `sandbox-runner`.
pub fn repo_slug(entry: &RepoEntry) -> Option<String> {
    let last = entry.remote(Role::Origin).rsplit('/').next()?;
    let trimmed = last.trim_end_matches(".git");
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
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

/// The release the next cut carries: the composition in hand.
///
/// Local-preferred under both schemes, because a cut names what is here —
/// including edits (`include`, `drop`, `advance`) not yet pushed. Reading the
/// publish remote instead once made a fixed-scheme cut duplicate the stale
/// published position and silently revert unpushed local edits.
/// [`previous_position`] remains the seam for what consumers observe.
pub fn previous_release_for_cut(
    entry: &RepoEntry,
    tips: &BookmarkTips,
) -> Option<(String, CommitId)> {
    let scheme = entry.release_scheme();
    newest_release(tips, &scheme, entry.publish_remote())
        .map(|(reference, commit)| (reference.to_string(), commit))
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

/// Every branch we hold: the current tip of each of them.
///
/// This is the *first* cut's membership — trunk plus exactly these — and after
/// that it is only the candidate set an `include` can be asked for, because a
/// later cut carries the composition in hand instead of recomputing it.
/// Explicit commit ids are read here, once, so a branch moving mid-cut cannot
/// change what gets merged.
pub fn carried_branches(
    repo: &Repo,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> anyhow::Result<Vec<(String, CommitId)>> {
    let tips = repo.bookmark_tips()?;
    Ok(carried_from_tips(&tips, trunk, scheme))
}

/// Pure seam so the trunk filter is testable without opening a repository.
pub fn carried_from_tips(
    tips: &BookmarkTips,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> Vec<(String, CommitId)> {
    tips.iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch)
                if !is_release_name(branch, scheme) && branch.as_str() != trunk =>
            {
                Some((branch.to_string(), commit.clone()))
            }
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        })
        .collect()
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
        short(&trunk)
    ))
}

/// The newest release we cut under this repository's configured scheme.
///
/// Uses the same ordering as `status`: otherwise those commands could report
/// different current releases for the same set of refs.
pub fn newest_release(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> Option<(BookmarkRef, CommitId)> {
    match scheme {
        ReleaseScheme::Dated => tips
            .iter()
            .filter(|(reference, _)| is_our_release(reference, scheme))
            // The same ordering `status` uses. These two commands answering "which
            // is the current release?" differently was a real divergence.
            .max_by_key(|(reference, _)| {
                (
                    crate::commands::status::release_order(reference.branch().as_str()),
                    // On a tie prefer the local ref, deterministically. `max_by_key`
                    // otherwise returns whichever came last in iteration order.
                    u8::from(reference.is_local()),
                )
            })
            .map(|(reference, commit)| (reference.clone(), commit.clone())),
        ReleaseScheme::Fixed(fixed) => tips
            .iter()
            .filter(|(reference, _)| match reference {
                BookmarkRef::Local(branch) => branch == fixed,
                BookmarkRef::Remote { branch, remote } => {
                    branch == fixed && remote.as_str() == publish_remote
                }
            })
            .max_by_key(|(reference, _)| u8::from(reference.is_local()))
            .map(|(reference, commit)| (reference.clone(), commit.clone())),
    }
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
/// trunk share. Falling back to the trunk tip here once charged all upstream
/// drift since the fork to the members: a published composition failed its own
/// re-cut, and `start` based new branches on the drifted tip.
pub fn shared_base(
    repo: &Repo,
    release: &CommitId,
    trunk_tip: &CommitId,
) -> anyhow::Result<Option<CommitId>> {
    let parents = repo.parents_of(release.as_str())?;
    let mut bases = Vec::new();
    for parent in &parents {
        if repo.is_ancestor(&parent.commit, trunk_tip)? {
            bases.push(parent.commit.clone());
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
    let members: Vec<CommitId> = parents.into_iter().map(|parent| parent.commit).collect();
    Ok(repo.common_ancestor(&members, trunk_tip)?)
}

/// Branches whose trunk ancestry exceeds the shared base (#10).
///
/// A branch based past the base drags newer upstream into the next cut through
/// itself alone, which surfaces as a conflict storm blamed on everything else.
/// The finding names the branch so the fix (rebase it onto the base, or move
/// the base deliberately) happens before the cut.
pub fn mixed_base_findings(
    repo_path: &Path,
    branches: &[(String, CommitId)],
    base: &CommitId,
    trunk_tip: &CommitId,
) -> Result<Vec<Finding>, crate::jj::JjError> {
    let mut findings = Vec::new();
    for (name, tip) in branches {
        let beyond = crate::jj::commits_matching(
            repo_path,
            &format!(
                "(::{tip} & ::{trunk}) ~ ::{base}",
                tip = tip.as_str(),
                trunk = trunk_tip.as_str(),
                base = base.as_str()
            ),
        )?;
        if !beyond.is_empty() {
            findings.push(Finding::new(
                FindingKind::MixedBase,
                Subject::Branch(crate::ids::BranchName::new(name)),
                format!(
                    "branch {name} carries {} trunk commit(s) beyond the shared base {}; \
                     it is based on a different upstream than that shared base",
                    beyond.len(),
                    short(base)
                ),
            ));
        }
    }
    Ok(findings)
}

/// Commits the recut would strand: reachable from the previous release or its
/// local descendants, and from no keeper.
///
/// Keepers are every non-release local bookmark tip plus the upstream trunk.
/// The previous cut itself, parked working copies, and commits identified by
/// our strict dated release bookmarks are excluded as release machinery, not
/// work. A legacy commit that only *describes* itself like release machinery
/// is deliberately reported: refusing it is safer than dropping real work.
pub fn orphaned_commits(
    repo_path: &Path,
    previous: &CommitId,
    keep: &[CommitId],
    tips: &BookmarkTips,
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let keepers = keep
        .iter()
        .map(|commit| commit.as_str().to_owned())
        .collect::<Vec<_>>()
        .join("|");
    let release_commits = our_dated_release_commits(tips);
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
pub fn our_dated_release_commits(tips: &BookmarkTips) -> BTreeSet<CommitId> {
    tips.iter()
        .filter(|(reference, _)| {
            is_our_release(reference, &ReleaseScheme::Dated)
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
pub fn superseded_dated_releases(tips: &BookmarkTips) -> Vec<(BookmarkRef, CommitId)> {
    let newest = tips
        .keys()
        // The vote is an allowlist, so someone else's cut cannot reap ours;
        // output remains broader to clean up our historical odd-remote refs.
        .filter(|reference| is_our_release(reference, &ReleaseScheme::Dated))
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
    /// Refs forgotten but the commit abandon refused (still pinned by a ref
    /// outside the enumeration, e.g. a tag); details in `notes`.
    pub forgotten_only: Vec<String>,
    /// (name, reason) pairs that were deliberately left alone.
    pub kept: Vec<(String, String)>,
    /// Non-fatal notes from abandon refusals.
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
pub fn reap_superseded(repo_path: &Path, repo: &Repo) -> anyhow::Result<ReapReport> {
    let tips = repo.bookmark_tips()?;
    let mut by_name = std::collections::BTreeMap::<String, Vec<CommitId>>::default();
    for (reference, commit) in superseded_dated_releases(&tips) {
        let targets = by_name.entry(reference.branch().to_string()).or_default();
        if !targets.contains(&commit) {
            targets.push(commit);
        }
    }
    if !by_name.is_empty()
        && let Some((live, commit)) = live_dated_release(&tips)
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
                let sample: Vec<String> = descendants.iter().take(3).map(short).collect();
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
            report
                .notes
                .push(format!("{name}: refs forgotten, abandon refused: {error}"));
            report.forgotten_only.push(name);
        }
    }
    Ok(report)
}

/// The newest dated cut's branch name — unqualified, unlike
/// [`previous_release_for_cut`]'s — and its live commit: the local ref when
/// present, otherwise whichever of our remotes carries it.
fn live_dated_release(tips: &BookmarkTips) -> Option<(BranchName, CommitId)> {
    let newest = tips
        .keys()
        .filter(|reference| is_our_release(reference, &ReleaseScheme::Dated))
        .filter_map(|reference| strict_dated_release(reference.branch().as_str()))
        .max()?;
    tips.iter()
        .filter(|(reference, _)| {
            is_our_release(reference, &ReleaseScheme::Dated)
                && strict_dated_release(reference.branch().as_str()).as_ref() == Some(&newest)
        })
        .max_by_key(|(reference, _)| u8::from(reference.is_local()))
        .map(|(reference, commit)| (reference.branch().clone(), commit.clone()))
}

pub fn plan(name: &RepoName, entry: &RepoEntry, consumers: &[PathBuf]) -> anyhow::Result<Plan> {
    let mut plan = Plan {
        repo: name.to_string(),
        ..Plan::default()
    };
    let repo = Repo::open(&entry.path)?;
    let tips = repo.bookmark_tips()?;

    // The newest release we cut. Historical ones are frozen and not our concern.
    let scheme = entry.release_scheme();
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
                    short(&parent.commit),
                    base.as_ref().map_or_else(String::new, short),
                ),
            ));
        }
    }
    plan.stale = stale_parents(&member_parents, &tips);
    // Branches a cut will not carry: membership is the release's parent set,
    // and a branch joins or moves only through a stated `include` or `advance`.
    // Saying so here is what keeps "it exists locally" from silently meaning
    // "it ships", without anyone having to remember to ask.
    let local_branches = carried_from_tips(&tips, entry.trunk(), &scheme);
    for (branch, tip) in &local_branches {
        if repo.is_ancestor(tip, &commit)? {
            continue;
        }
        // A branch built on top of the release descends from every member, which
        // ancestry alone reads as "advanced". It is neither advanced nor
        // includable as it stands: both verbs refuse it, because carrying it would
        // put the cut in its own successor's ancestry.
        let note = if repo.is_ancestor(&commit, tip)? {
            format!(
                "{branch} is stacked on {reference} rather than the trunk; rebase it off the \
                 trunk before including it"
            )
        } else if any_ancestor_of(&repo, &member_parents, tip)? {
            format!(
                "{branch} has advanced past its parent in {reference}; \
                 `knives release advance {branch}` moves it"
            )
        } else {
            format!("{branch} is not in {reference}; `knives release include {branch}` adds it")
        };
        plan.notes.push(note);
    }
    if let (Some(base), Some(trunk)) = (&base, &trunk_tip) {
        let findings = mixed_base_findings(&entry.path, &local_branches, base, trunk)?;
        plan.base_findings.extend(findings);
    }
    plan.parents = parents
        .into_iter()
        .map(|parent| {
            let names = parent.bookmarks.iter().map(ToString::to_string).collect();
            (parent.commit, names)
        })
        .collect();

    if consumers.is_empty() {
        // The command's central question. Unanswered is not success.
        plan.problems.push(
            "no consumers recorded, so pinned-ness is unknown; add `consumers = [...]` to \
             the registry entry, or pass --consumer"
                .to_owned(),
        );
    }
    // Every consumer, not one: they can sit on different releases, so a plan that saw only
    // the first would call a release unpinned while something else was frozen on it.
    let slug = repo_slug(entry);
    for consumer in consumers {
        let (pins, notes) = scan_consumer_for(consumer, slug.as_deref(), &scheme);
        plan.pins.extend(pins);
        plan.notes.extend(notes);
    }
    Ok(plan)
}

/// Whether any of `parents` is an ancestor of `tip`.
///
/// A loop rather than `any`, because an ancestry jj cannot answer has to
/// surface as an error: read as "no" it would label an advanced branch as
/// never included, and send someone to `include` where `advance` is the answer.
fn any_ancestor_of(repo: &Repo, parents: &[ReleaseParent], tip: &CommitId) -> anyhow::Result<bool> {
    for parent in parents {
        if repo.is_ancestor(&parent.commit, tip)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn short(commit: &CommitId) -> String {
    commit.as_str().chars().take(12).collect()
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
    lines.push(format!("  {} parent(s), flat", plan.parents.len()));
    for (commit, names) in &plan.parents {
        let held = if names.is_empty() {
            "no bookmark".to_owned()
        } else {
            names.join(", ")
        };
        lines.push(format!("    {}  {held}", short(commit)));
    }

    if plan.stale.is_empty() && plan.base_findings.is_empty() {
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

    lines.push("  pinned by:".to_owned());
    lines.push(crate::pins::render(&plan.pins));
    lines.push(match repair_effect(&plan.pins) {
        RepairEffect::RepairInPlace => {
            "  at least one consumer follows the branch: repair in place, no new dated name"
                .to_owned()
        }
        RepairEffect::NewDatedName => {
            "  every pin is frozen: the next cut needs a new dated suffix".to_owned()
        }
        RepairEffect::Unpinned => "  nothing pins this release: either is safe".to_owned(),
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

        let superseded = superseded_dated_releases(&tips);
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
        let names: Vec<String> = superseded_dated_releases(&tips)
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
        let names: Vec<String> = superseded_dated_releases(&tips)
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
        let names: Vec<String> = superseded_dated_releases(&tips)
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
        let names: Vec<String> = superseded_dated_releases(&tips)
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
            source: String::new(),
        }
    }

    #[test]
    fn one_following_consumer_means_repair_in_place() {
        // A needless dated name burns the name and forces a re-pin nobody wanted.
        let pins = vec![pin(PinKind::Frozen), pin(PinKind::Follows)];
        assert_eq!(repair_effect(&pins), RepairEffect::RepairInPlace);
    }

    #[test]
    fn all_frozen_means_a_new_dated_name() {
        assert_eq!(
            repair_effect(&[pin(PinKind::Frozen)]),
            RepairEffect::NewDatedName
        );
    }

    #[test]
    fn nothing_pinning_it_leaves_the_choice_open() {
        assert_eq!(repair_effect(&[]), RepairEffect::Unpinned);
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
    /// The message a cut carries, provenance included.
    pub fn message(&self) -> String {
        let mut lines = vec![format!("release: {}", self.name), String::new()];
        for (commit, source) in &self.provenance {
            lines.push(format!("parent {} from {source}", commit.as_str()));
        }
        lines.join("\n")
    }
}

/// The post-construction checks that determine whether a cut is safe to name.
#[derive(Debug, Default)]
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

/// Build the candidate cut and verify it has exactly the parents asked for.
/// Public seam so the audit can run between creation and naming.
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
/// This never pushes. Publishing a release is a separate, deliberate act.
pub fn build_cut(
    repo: &Path,
    request: &Cut,
    previous: Option<&CommitId>,
) -> anyhow::Result<CommitId> {
    let message = request.message();
    let created = crate::jj::write_release(
        repo,
        &crate::jj::ReleaseWrite {
            source: previous,
            parents: &request.parents,
            message: Some(&message),
            bookmark: None,
            operation: &format!("knives: cut {}", request.name),
        },
    )?;
    let actual = Repo::open(repo)?.parents_of(created.as_str())?;
    anyhow::ensure!(
        actual.len() == request.parents.len(),
        "cut {} came out with {} parents, expected {}; refusing to name it",
        request.name,
        actual.len(),
        request.parents.len()
    );
    Ok(created)
}

/// Point the release name at an already-checked merge.
///
/// Dated cuts retain ordinary bookmark-movement protection because each name
/// records a new release. Fixed cuts deliberately move their existing release
/// name, which may already exist on the remote.
pub fn name_cut(
    repo: &Path,
    name: &str,
    commit: &CommitId,
    scheme: &ReleaseScheme,
) -> anyhow::Result<()> {
    match scheme {
        ReleaseScheme::Dated => crate::jj::set_bookmark(repo, name, commit.as_str())?,
        ReleaseScheme::Fixed(_) => {
            crate::jj::set_bookmark_anywhere(repo, name, commit.as_str())?;
        }
    }
    Ok(())
}

/// Verify the cut actually contains what it merged (spec 1.3).
///
/// For each member, a scratch child of `trunk` is restored to the member tip's
/// tree, producing a synthetic commit whose diff is `trunk..member_tip`; that
/// single net commit is replayed onto the fresh cut.
/// An empty replay means its hunks are present; a clean, non-empty replay means
/// the cut silently lacks them. A conflicted replay is inconclusive only when
/// the cut itself has unresolved conflicts; otherwise its tree diverges from the
/// member and fails the audit.
/// Changes from the previous release that no member or trunk explains are
/// merge-invented drift.
pub fn audit_cut(
    repo: &Path,
    members: &[(String, CommitId)],
    cut: &CommitId,
    context: AuditContext<'_>,
) -> anyhow::Result<CutAudit> {
    let mut audit = CutAudit::default();
    let cut_is_conflicted = !crate::jj::conflicted_files(repo, cut.as_str())?.is_empty();
    for (name, tip) in members {
        match crate::jj::probe_net_diff(repo, context.trunk.as_str(), tip.as_str(), cut.as_str())? {
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
        let drifted = crate::jj::changed_files_between(repo, previous.as_str(), cut.as_str())?;
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
    let created = build_cut(repo, request, None)?;
    name_cut(repo, &request.name, &created, scheme)?;
    Ok(created)
}

#[cfg(test)]
mod cut_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

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
pub fn check_test_count(
    repo: &Path,
    entry: &RepoEntry,
    cut: &CommitId,
    parent: &CommitId,
) -> TestCountCheck {
    let Some(command) = entry.test_count_command.as_deref() else {
        return TestCountCheck::NotConfigured;
    };
    let merged = crate::jj::output_at_revision(repo, cut.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    let single = crate::jj::output_at_revision(repo, parent.as_str(), command)
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

        let (ours, notes) =
            scan_consumer_for(dir.path(), Some("sandbox-runner"), &ReleaseScheme::Dated);
        assert_eq!(ours.len(), 1, "only our own pin: {ours:?}");
        assert_eq!(ours[0].reference, "release/2026-07-20");
        assert_eq!(
            notes,
            vec![format!(
                "{}: not a repository; pins read from the working copy",
                dir.path().display()
            )]
        );

        let (unscoped, _) = scan_consumer_for(dir.path(), None, &ReleaseScheme::Dated);
        assert_eq!(unscoped.len(), 2, "without a slug, every pin is kept");
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
