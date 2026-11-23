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
//! of them writes the release locally only, and none of them pushes it.
// allow: SIZE_OK: 5774 lines - the release lifecycle's plan, members, cut, edit, adopt refusals and remedies, audit, reap, and rebase operations, and the pull every one of them plans from, are one domain seam.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::bind::Fork;
use crate::cli::Exit;
use crate::config::{Registry, RepoEntry};
use crate::consumer_pins::{
    ConsumerHeadMemo, ConsumerPinSource, scan_consumer_for, scan_consumer_slug_with_heads,
};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, ReleaseParent, Subject, release_parents, stale_parents,
};
use crate::ids::{
    BookmarkRef, BranchName, ChangeId, CommitId, ReleaseScheme, RemoteName, RepoName,
    is_our_dated_cut, is_our_release, is_release_name,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, Kind, Ledger, RecordedParent};
use crate::own_lines::{Lost, LostKind, MemberLines, Reading, SkippedRange};
use crate::pins::{Pin, PinKind};
use crate::release_model::{
    BranchSuccessions, CompositionBaseline, ConsumerScan, EarlierWrites, MemberEvidence,
    MemberHistory, MemberSuccession, RecordedWrite, SeriesWrite, StackedHistoryContext,
    carried_from_tips, double_cut_findings, last_recorded_parents, matched_write, member_history,
    member_parents, newest_release, read_release, recorded_release_changes, release_refs_by_commit,
    repo_slug, trunk_positions,
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

/// What a release command asks the network for: the fork's ledger, pulled,
/// and each registered consumer's pins, read from the forge ([`fetch`]).
///
/// A release write asks for these before it takes its fork's lock, and reads
/// the rest under it ([`Fetched::plan`]), so a pull or a forge call that
/// stalls keeps no other writer of the fork waiting. Neither answer is the
/// fork's lock's to guard: another machine's entries come in by a pull
/// whichever lock is held here, and a consumer's pins are the consumer's.
#[derive(Debug)]
pub struct Fetched {
    /// What the pull could not bring.
    pulled: Vec<String>,
    /// Each registered consumer's scan, in the registry's order.
    scans: Vec<ConsumerScan>,
}

/// Every network call a release plan makes ([`Fetched`]).
///
/// It pulls `fork`'s ledger from every destination `registry` gives it, and
/// scans each consumer `consumers` registers on the forge. A pull that fails,
/// and a consumer that could not be read, are problems of the plan.
pub fn fetch(fork: &Fork<'_>, registry: &Registry, consumers: &ConsumerInputs<'_>) -> Fetched {
    let pulled =
        crate::ledger_sweep::pull(registry, &[&fork.upstream]).problems_for(&fork.upstream);
    Fetched {
        pulled,
        scans: scan_consumers(fork, consumers),
    }
}

impl Fetched {
    /// The release plan for `fork`, from its ledger and repository as they
    /// are now, with the consumer pins [`fetch`] read.
    ///
    /// It is what a release command decides from; a writer reads it under its
    /// fork's lock, after the network.
    ///
    /// A pull that failed is one of the plan's problems, and so is each entry
    /// or statement the ledger read could not fully read. The plan still
    /// answers from the entries this machine has, and a command that would
    /// write from it refuses, exactly as it does when a consumer could not be
    /// consulted: the drop guard checks a member against the newest recorded
    /// cut, and a stale ledger would check it against the wrong one.
    pub fn plan(self, fork: &Fork<'_>, consumers: &ConsumerInputs<'_>) -> anyhow::Result<Plan> {
        let read = Ledger::for_fork(fork).read()?;
        let mut plan = plan_scanned(fork, consumers, &read.entries, Some(self.scans))?;
        plan.problems.extend(self.pulled);
        plan.problems.extend(read.problems);
        Ok(plan)
    }
}

/// Each consumer `consumers` registers on the forge, scanned for `fork`'s
/// release pins, in the registry's order.
fn scan_consumers(fork: &Fork<'_>, consumers: &ConsumerInputs<'_>) -> Vec<ConsumerScan> {
    let scheme = fork.entry.release_scheme();
    let slug = repo_slug(fork.entry);
    consumers
        .slugs
        .iter()
        .map(|consumer| {
            scan_consumer_slug_with_heads(
                consumers.forge,
                consumers.cache_root,
                &fork.checkout.path,
                consumer,
                slug.as_deref(),
                &scheme,
                consumers.heads,
            )
        })
        .collect()
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

/// Whether a `record: published …` note on `release` names `commit`: the
/// note half of [`published_release`], read on that one commit and under
/// every release scheme.
pub fn published_note_names(ledger: &[Entry], release: &str, commit: &CommitId) -> bool {
    published_note(ledger, release, &[commit]).is_some()
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
/// it would never be the newest cut, and the reap would never take it. A
/// label git cannot hold in a ref, such as `-x~1`, is one of those, and the
/// refusal says why ([`ReleaseScheme::ref_flaw`]): no ref could publish it.
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
            Err(scheme.ref_flaw(name).map_or_else(
                || {
                    format!(
                        "{name} is not a name this series can sort; its cuts are named {}, \
                         where the date is followed by nothing, by `.N` or by `-<label>`, e.g. {}",
                        scheme.cut_shape(),
                        example()
                    )
                },
                |why| {
                    format!(
                        "{name} cannot name a git ref ({why}); its cuts are named {}, where a \
                         `-<label>` holds nothing git refuses in a ref name, e.g. {}",
                        scheme.cut_shape(),
                        example()
                    )
                },
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
        // A label git cannot hold in a ref is refused with the grammar and
        // the rule it breaks: no ref could ever publish it.
        let error = cut_name(&old, Some("release/9.2/2026-10-21-x~1")).unwrap_err();
        assert!(
            error.contains("cannot name a git ref (it contains one of space ~ ^ : ? * [ \\)")
                && error.contains("release/9.2/YYYY-MM-DD[.N|-<label>]"),
            "{error}"
        );
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
    /// ([`crate::release_model::EarlierWrites::carriers`]): listed among
    /// `parents`, labelled, and read by no member check.
    pub carriers: Vec<CommitId>,
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
    /// parents every member check reads.
    ///
    /// The release reads as [`read_release`] reads it, its carriers read into
    /// [`Plan::carriers`] from `ledger` and [`Plan::published`]. A carrier is
    /// an earlier write of the release a fast-forward push gave it, never a
    /// member: no member check reads it, and the parent list labels it. The
    /// list is the release's parents, its carriers among them.
    fn read_parents(
        &mut self,
        (repo, ledger): (&Repo, &[Entry]),
        (reference, commit): (&BookmarkRef, &CommitId),
    ) -> anyhow::Result<(Vec<ReleaseParent>, Vec<ReleaseParent>)> {
        let release = reference.branch().as_str();
        let writes = EarlierWrites::read(repo, release, ledger, self.published.as_ref())?;
        let reading = read_release(repo, (&writes, commit))?;
        let parents = repo.parents_of(commit.as_str())?;
        let members = parents
            .iter()
            .filter(|parent| reading.members.contains(&parent.commit))
            .cloned()
            .collect();
        self.carriers = reading.carriers;
        Ok((parents, members))
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

    /// The plan's parent list, one line per parent with what holds it, a
    /// carrier of `release` labelled as one. Bookmarks are names read from
    /// the repository, so each is escaped.
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
                if self.carriers.contains(commit) {
                    format!(
                        "    {}  {} ({held})",
                        commit.short(),
                        inline_human_text(&label)
                    )
                } else {
                    format!("    {}  {held}", commit.short())
                }
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
/// Every tip of a non-release bookmark that is local or of the fork's own
/// remotes, `origin` and the publish remote, tracked or not: the fork's
/// branches are on `origin` and its releases on the publish remote, so work
/// a branch there holds is not lost when the cut drops it. A bookmark of
/// `upstream` or of any other remote keeps nothing: its owner can delete it.
/// Every commit a divergent such bookmark names (it has no single tip, but
/// each target is still bookmarked work — a member whose bookmark also
/// pointed at its merged pull request head read as dropped by the cut that
/// carried it); the previous release's own parents (the cut carries them
/// verbatim by commit id, so they are kept by construction whatever their
/// bookmarks are doing); and the upstream trunk, `trunk`.
pub fn cut_keepers(
    repo: &Repo,
    entry: &RepoEntry,
    tips: &BookmarkTips,
    previous: &CommitId,
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let scheme = &entry.release_scheme();
    let publish_remote = entry.publish_remote();
    let keeps = |reference: &BookmarkRef| {
        !is_release_name(reference.branch(), scheme)
            && match reference {
                BookmarkRef::Local(_) => true,
                BookmarkRef::Remote { remote, .. } => {
                    remote.as_str() == "origin" || remote.as_str() == publish_remote
                }
            }
    };
    let mut keep: Vec<CommitId> = tips
        .iter()
        .filter(|&(reference, _)| keeps(reference))
        .map(|(_, commit)| commit.clone())
        .collect();
    for (reference, commits) in repo.conflicted_bookmarks()? {
        if keeps(&reference) {
            keep.extend(commits);
        }
    }
    keep.extend(repo.parent_commits(previous.as_str())?);
    keep.push(repo.resolve_commit(&entry.trunk_ref())?);
    // A remote's tip is often the local bookmark's commit: the callers join
    // the list into one revset, which needs each commit once.
    keep.sort();
    keep.dedup();
    Ok(keep)
}

/// Commits the recut would strand: reachable from the previous release or
/// from any of its visible descendants, and from no keeper.
///
/// A descendant counts whatever holds it: a local bookmark, a remote branch,
/// tracked or not, or nothing at all. Keepers are [`cut_keepers`]. The
/// previous cut itself, empty undescribed working copies,
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

/// Each superseded dated cut's name ([`superseded_dated_releases`]) with the
/// commits its refs name, each once, in ref order.
fn superseded_by_name(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> std::collections::BTreeMap<String, Vec<CommitId>> {
    let mut by_name = std::collections::BTreeMap::<String, Vec<CommitId>>::default();
    for (reference, commit) in superseded_dated_releases(tips, scheme, publish_remote) {
        let targets = by_name.entry(reference.branch().to_string()).or_default();
        if !targets.contains(&commit) {
            targets.push(commit);
        }
    }
    by_name
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
    /// the commit stays reachable by its pin. Nothing else lands here; an
    /// abandon that failed for another reason is a note.
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

/// A release name the reap must keep for its claim.
///
/// It is one a claim in this machine's state file holds, which `knives start
/// <name>` takes and `knives finish <name>` releases, or one whose lock a
/// `start` or `finish` of it holds now ([`crate::store::BranchLock`]), which
/// may be about to take or release that claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRelease {
    pub name: String,
    /// The claim's owner; `None` for a name a `start` or `finish` holds now.
    pub holder: Option<String>,
}

/// The live release claims of a fork, as the reap reads them from the state
/// file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseClaims {
    /// The claims of the fork's release names.
    Read(Vec<HeldRelease>),
    /// The state file could not be read, and why.
    Unreadable(String),
}

impl ReleaseClaims {
    /// Why the reap keeps the superseded `name` for a release claim, or
    /// `None` when no claim holds it. Every name is kept while the claims
    /// cannot be read.
    fn keep(&self, name: &str) -> Option<String> {
        match self {
            Self::Unreadable(error) => Some(format!(
                "the claims could not be read, so a release claim may hold it: {error}"
            )),
            Self::Read(held) => held.iter().find(|claim| claim.name == name).map(|claim| {
                claim.holder.as_ref().map_or_else(
                    || {
                        "a `knives start` or `finish` of it runs now; a later reap \
                                 retires it"
                            .to_owned()
                    },
                    |holder| {
                        format!(
                            "held by the release claim of {holder}; `knives finish {}` \
                                     releases it",
                            crate::ledger::shell_word(name)
                        )
                    },
                )
            }),
        }
    }
}

/// [`reap_superseded_held`] where no release claim holds a name.
pub fn reap_superseded(
    repo: &Repo,
    scheme: &ReleaseScheme,
    publish_remote: &str,
    ledger: &[Entry],
) -> anyhow::Result<ReapReport> {
    reap_superseded_held(
        repo,
        scheme,
        publish_remote,
        ledger,
        &ReleaseClaims::Read(Vec::new()),
    )
}

/// Reap every superseded dated cut: forget its refs everywhere, abandon its commits.
///
/// The newest dated name never appears in the enumeration, and the
/// `previous_position` seam is `Fixed`-scheme-only while dated names are the
/// only thing enumerated, so neither needs a runtime gate here. Three things
/// do: a cut that anything rests on is kept with what rests on it named, by
/// the rule a release edit keeps the commit it replaced by
/// ([`Repo::what_rests_on`]); a name a live release claim holds is kept with
/// its holder named (`claims`); and while the live cut still carries
/// conflicts, every superseded cut is kept, because the previous cut is the
/// only record of how those conflicts were last resolved — reaping it while
/// the successor is unsettled destroys the record exactly when an
/// abandon-and-recut would need it.
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
/// A release claim is the write lease of a release's refs, and its
/// workspace rests on the root commit, so nothing else keeps a claimed name.
/// Its keep names `knives finish <name>` and never `--force`: the holder the
/// state file names can be this machine's copy of a ledger claim the ledger
/// no longer holds, and a forced finish would then release the live
/// holder's claim. Where the claims could not be read, every superseded name
/// is kept, because a name reaped then may be one a claim holds.
///
/// Never touches a remote. A later fetch re-materializes forgotten refs as
/// untracked (jj keeps no memory of forgetting); that is expected, harmless to
/// the default log, and cleared by the next reap. Correctness never depends on
/// reaping having run: the divergence detector ignores these refs regardless.
#[allow(
    clippy::too_many_arguments,
    reason = "the repository, the series, the publish remote, the ledger and the claims are independent inputs"
)]
pub fn reap_superseded_held(
    repo: &Repo,
    scheme: &ReleaseScheme,
    publish_remote: &str,
    ledger: &[Entry],
    claims: &ReleaseClaims,
) -> anyhow::Result<ReapReport> {
    let repo_path = repo.path();
    let tips = repo.bookmark_tips()?;
    let by_name = superseded_by_name(&tips, scheme, publish_remote);
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
        if let Some(reason) = claims.keep(&name) {
            report.kept.push((name, reason));
            continue;
        }
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
    /// members, whether or not `write`'s own event is marked unchecked. Every
    /// write refuses it until `adopt` records it.
    Unrecorded { write: RecordedWrite },
    /// Nothing here can measure the release against the writes the ledger
    /// records of its name, or it holds a recorded write that nothing
    /// measured ([`Unchecked`]). A write over it proceeds and says so, except
    /// over a release whose every parent is an earlier write
    /// ([`NoMatch::AllCarriers`], whether or not the ledger records a write of
    /// the name), which every write and `adopt` refuse.
    Unchecked(Unchecked),
    /// The ledger records no write of the name: a release older than the
    /// ledger, or one made with no knives at all. No write refuses it, and
    /// nothing measured it, so a write over it proceeds, says so, and marks
    /// its event unchecked ([`crate::release_model::UNCHECKED_NO_RECORD`]).
    /// A later write over that marked write, or over a jj rebase or metadata
    /// rewrite of it, inherits the mark ([`Unchecked::Inherited`]) until
    /// `adopt` records it; a change by hand on top of it is
    /// [`Self::Unrecorded`], which every write refuses until then. `adopt` has
    /// no write of the release itself to replace, so a cut's carried rows say
    /// the ledger records no write of it and that a person must check each
    /// divergence by hand ([`CarriedFrom::Unledgered`]). A release whose every
    /// parent is an earlier write is [`Self::Unchecked`] instead.
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
    /// It is the recorded write it equals, or a metadata rewrite of it, or its
    /// tree is that write rebased onto its members
    /// ([`Repo::holds_rebased_write`]), and that write's own event says knives
    /// could not check the release it was written over (`reason`). Nothing
    /// measured that release since, so it stays unchecked until `adopt`
    /// records it. A change by hand on top of that write is
    /// [`InHandWrite::Unrecorded`] instead.
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
/// proceeds as it always has, saying so.
///
/// A matched write whose own event is marked unchecked is held to its tree
/// the same way. A release that is that write, or a jj rebase or metadata
/// rewrite of it, stays unchecked with the write's mark
/// ([`Unchecked::Inherited`]), so a write over a conflict it left unresolved
/// goes ahead. A release whose tree carries a change on top of it, such as
/// that conflict resolved by hand, is unrecorded and every write refuses it
/// until `adopt` records it.
pub fn in_hand_write(release: &InHand<'_>) -> Result<InHandWrite, crate::jj::JjError> {
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
            return Ok(unless_marked(event, write));
        }
    }
    // A release whose every parent is a carrier has no member to compose
    // from, so the in-between commit a write builds never stands on no
    // parent, whether or not the ledger records a write of the name.
    let reading = read_release(repo, (writes, commit))?;
    if let Some(into) = reading.carriers.first()
        && reading.members.is_empty()
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
    // The tree decides before the mark: a change on top of the matched write
    // is unrecorded even where the write's own event is marked unchecked, and
    // every write refuses it until `adopt`. A release that holds a marked
    // write, rebased onto its members, stays unchecked with that write's mark
    // ([`Unchecked::Inherited`]).
    let composed = write_composition(repo, (writes, &matched.write.commit))?;
    if !repo.holds_rebased_write((&matched.write.commit, &composed), &reading.members, commit)? {
        return Ok(InHandWrite::Unrecorded {
            write: matched.write,
        });
    }
    Ok(unless_marked(matched.event, matched.write))
}

/// What `write`, a recorded write of a release whose earlier writes are
/// `writes`, composes: the commits whose merge its own change is measured
/// against ([`Repo::holds_rebased_write`]).
///
/// That is its own parents, its carriers left out
/// ([`EarlierWrites::recorded_carriers`]).
pub fn write_composition(
    repo: &Repo,
    (writes, write): (&EarlierWrites, &CommitId),
) -> Result<Vec<CommitId>, crate::jj::JjError> {
    let parents = repo.parent_commits(write.as_str())?;
    let carriers = writes.recorded_carriers(repo, &parents)?;
    Ok(parents
        .into_iter()
        .filter(|parent| !carriers.contains(parent))
        .collect())
}

/// How a release in hand that holds `write` stands, unless `event`, the
/// write's own, says knives could not check the release it was written over:
/// then the release is unchecked too ([`Unchecked::Inherited`]), since
/// nothing measured what it stands on. Only a release that is `write`, or a
/// jj rebase or metadata rewrite of it, gets here: one whose tree carries a
/// change on top of `write` is [`InHandWrite::Unrecorded`], marked or not.
fn unless_marked(event: &Entry, write: RecordedWrite) -> InHandWrite {
    crate::release_model::unchecked_mark(event).map_or(InHandWrite::Recorded, |reason| {
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
    // move or for adopt to read, so it is tracked first. The name is one
    // shell word inside the command; the remote is a registry value.
    let track = if repo.local_bookmark_tip(name).is_none() {
        format!(
            "track it (`jj bookmark track {}@{publish_remote}`), then ",
            crate::ledger::shell_word(name)
        )
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
    // The release is a name read from the repository: escaped in prose, and
    // one shell word inside the command.
    let shown = crate::ledger::inline_human_text(name);
    let at = commit.short();
    let adopt = format!(
        "`knives release adopt {}` records it",
        crate::ledger::shell_word(name)
    );
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
/// publish remote lacks, the cut leaves R behind. It then takes
/// `--allow-drop`, names each commit the orphan gate would drop, R first,
/// and must find them still visible. A bookmark keeps a commit off that
/// list: a local one, or a branch of `origin` or of the publish remote,
/// tracked or not, whose name is not a release name ([`cut_keepers`]). With
/// nothing to drop, the cut takes no `--allow-drop`. Where the publish
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
            " (`jj bookmark set {} -r {} --allow-backwards`)",
            crate::ledger::shell_word(name),
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
        "`jj bookmark forget {}` (never `jj bookmark delete`, which a push would carry to \
         the remote)",
        crate::ledger::shell_word(name)
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
    // The release is a name read from the repository: escaped in prose, and
    // one shell word inside the command.
    let shown = crate::ledger::inline_human_text(name);
    Ok(Some(format!(
        "{shown} at {} carries a change no knives write or adopt recorded: its tree is not {} \
         rebased onto its members; record it with `knives release adopt {}` (adopt asks for \
         --why only for a change outside the conflicts)",
        commit.short(),
        described_write(write),
        crate::ledger::shell_word(name)
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

/// What a cut's drift is measured from, which its unexplained rows name
/// ([`unexplained_row`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftBase {
    /// The release the cut follows: a cut's previous release, or the release
    /// a record follows.
    PreviousRelease,
    /// The members' fork point with the trunk: the series' first record
    /// outside knives follows no release.
    ForkPoint,
}

/// The row for a file the audited release changed since `base` that no
/// member or trunk explains ([`CutAudit::unexplained`]).
pub fn unexplained_row(file: &str, base: DriftBase) -> String {
    // A path jj's diff named, so it reaches the terminal escaped.
    let file = crate::ledger::inline_human_text(file);
    let base = match base {
        DriftBase::PreviousRelease => "the previous release",
        DriftBase::ForkPoint => "the members' fork point with the trunk",
    };
    format!(
        "  !! {file}: changed between {base} and this cut with no member or trunk \
         explaining it"
    )
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
/// landed upstream, where the repository no longer can. Each consumer the
/// registry names is scanned on the forge here; a writer scans them first,
/// before its fork's lock, and plans from those ([`Fetched::plan`]).
pub fn plan(
    fork: &Fork<'_>,
    consumers: &ConsumerInputs<'_>,
    ledger: &[Entry],
) -> anyhow::Result<Plan> {
    plan_scanned(fork, consumers, ledger, None)
}

/// [`plan`], with each registered consumer's scan already made where `scans`
/// holds them, in the registry's order.
fn plan_scanned(
    fork: &Fork<'_>,
    consumers: &ConsumerInputs<'_>,
    ledger: &[Entry],
    scans: Option<Vec<ConsumerScan>>,
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

    let (parents, members) = plan.read_parents((&repo, ledger), (&reference, &commit))?;
    let trunks = trunk_positions(&repo, entry)?;
    let trunk_tip = repo.resolve_commit(&entry.trunk_ref()).ok();
    let member_parents =
        unlanded_members(&repo, (&commit, trunk_tip.as_ref()), &members, &mut plan)?;
    let releases = release_refs_by_commit(&tips, &scheme, publish_remote);
    let in_hand = [commit.clone()];
    let written = recorded_release_changes(&repo, ledger, &scheme);
    let stacked_context = (!trunks.is_empty()).then_some(StackedHistoryContext {
        repo: &repo,
        trunks: &trunks,
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
        branches: &series_branches,
        recorded: last_recorded_parents(ledger, &reference.to_string()),
    })?);
    plan.list_parents(parents);

    add_consumer_pins(&mut plan, fork, consumers, scans);
    Ok(plan)
}

/// Every consumer's pins: each registered one's scan, `scans` where the
/// caller made them, and each local checkout's.
fn add_consumer_pins(
    plan: &mut Plan,
    fork: &Fork<'_>,
    consumers: &ConsumerInputs<'_>,
    scans: Option<Vec<ConsumerScan>>,
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
    let scheme = fork.entry.release_scheme();
    let slug = repo_slug(fork.entry);
    let consulted = plan.problems.len();
    for scan in scans.unwrap_or_else(|| scan_consumers(fork, consumers)) {
        plan.pins.extend(scan.pins);
        plan.notes.extend(scan.notes);
        plan.problems.extend(scan.problems);
    }
    for consumer in consumers.locals {
        let scan = scan_consumer_for(consumer, slug.as_deref(), &scheme);
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
    branches: &'a [(String, CommitId)],
    /// The release's last recorded parent set, for a branch the repository can
    /// no longer tie to its parent.
    recorded: &'a [RecordedParent],
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
        // The branch is one shell word inside each command a person pastes.
        let word = crate::ledger::shell_word(branch);
        notes.push(match (lookup.parents.first(), lookup.evidence) {
            (None, _) => {
                format!("{branch} is not in {reference}; `knives release include {word}` adds it")
            }
            (Some(_), MemberEvidence::Succession) if integrates => format!(
                "{branch} merges {} member(s) of {reference} and builds on them; `knives \
                 release include {word}` adds it beside them",
                lookup.parents.len()
            ),
            (Some(_), MemberEvidence::Succession) => format!(
                "{branch} has advanced past its parent in {reference}; \
                 `knives release advance {word}` moves it"
            ),
            (Some(parent), MemberEvidence::Record) => format!(
                "{branch} was released as {} in {reference} and has since been rebased outside \
                 jj; `knives release advance {word}` moves it through that record",
                parent.short()
            ),
            (Some(parent), MemberEvidence::LandedRecord) => format!(
                "{branch}'s released parent {} in {reference} has landed upstream and the \
                 branch kept going; `knives release advance {word}` moves it, \
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
    /// An earlier write of the release a fast-forward push gave it
    /// ([`crate::release_model::EarlierWrites::carriers`]): no member, and
    /// labelled so.
    pub carrier: bool,
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
    /// `--verify` only: `audit_cut`'s four buckets, reusing the cut path's
    /// phrasing, and each member's lines ([`CutAudit::members`]).
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
    })
}

/// The rows of a release's direct parents, each carrier among them
/// ([`EarlierWrites::carriers`]) labelled.
fn parent_rows(
    opened: &Repo,
    (parents, writes): (Vec<ReleaseParent>, &EarlierWrites),
    (trunk, successions): (Option<&CommitId>, &BranchSuccessions<'_>),
) -> anyhow::Result<Vec<MemberRow>> {
    let parent_commits: Vec<CommitId> =
        parents.iter().map(|parent| parent.commit.clone()).collect();
    let carriers = writes.carriers(&parent_commits);
    parents
        .into_iter()
        .map(|parent| {
            let carrier = carriers.contains(&parent.commit);
            member_row(opened, (parent, carrier), trunk, successions)
        })
        .collect()
}

/// Whether a row is one the content audit holds the release to: neither a
/// legacy cut's base nor an earlier write of the release.
const fn is_member(member: &MemberRow) -> bool {
    !member.base_parent && !member.carrier
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
/// ([`EarlierWrites`]). `verify` reads each member's own lines (`--verify`),
/// each from the member's own fork point with the upstream trunk, against no
/// previous release: a member whose lines the tree does not hold reads as
/// missing, however the release came to drop them.
pub fn gather_members(
    opened: &Repo,
    fork: &Fork<'_>,
    name: &str,
    (ledger, verify): (&[Entry], bool),
) -> anyhow::Result<MembersReport> {
    let entry = fork.entry;
    let commit = opened.resolve_commit(name)?;
    let parents = opened.parent_commits(commit.as_str())?;
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
    let writes = EarlierWrites::read(opened, release.as_str(), ledger, published)?;
    let members = parent_rows(
        opened,
        (release_parents(&tips, &parents), &writes),
        (trunk.as_ref(), &successions),
    )?;
    let audit = if verify {
        trunk
            .as_ref()
            .map(|trunk| {
                let members: Vec<(String, CommitId)> = members
                    .iter()
                    .filter(|member| is_member(member))
                    .map(|member| (member_label(member), member.commit.clone()))
                    .collect();
                audit_cut(
                    &fork.checkout.path,
                    &members,
                    CutSubject::Committed(&commit),
                    AuditContext {
                        previous: None,
                        upstream_trunk: trunk,
                        trunk_parents: &[],
                        trunk_parent_base: None,
                        drift_trunk: trunk,
                        drift_from: None,
                    },
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
        if member.carrier {
            description = if description.is_empty() {
                inline_human_text(&label)
            } else {
                format!("{} ({description})", inline_human_text(&label))
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
                "  {}: content check inconclusive (an unresolved conflict; re-check after \
                 resolving it)",
                inline_human_text(name)
            ));
            lines.extend(audit.lost_lines_of(name));
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
            lines.extend(audit.lost_lines_of(name));
        }
        // A file that changed since the release the audit measured from, with
        // no member or trunk explaining it.
        lines.extend(
            audit
                .unexplained
                .iter()
                .map(|file| unexplained_row(file, DriftBase::PreviousRelease)),
        );
        lines.extend(audit.words_lines());
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
    /// Members the cut lacks only lines of that the previous release already
    /// lacked ([`MemberLines::carry_forward`]). Reported, never refused —
    /// the audit charges a cut only with divergence it introduces. Whether
    /// that divergence is a recorded resolution is the caller's question:
    /// each row says how the previous release stands to its recorded writes
    /// ([`carried_row`]).
    pub carried: Vec<String>,
    /// Each member's own lines against the cut, carried or not, each from its
    /// fork point with the upstream trunk ([`audit_cut`]): what the cut
    /// lacks of them, and the lines it holds only by their words.
    pub members: Vec<MemberLines>,
}

impl CutAudit {
    pub const fn passed(&self) -> bool {
        self.missing.is_empty() && self.unexplained.is_empty()
    }

    /// The lines that name what the member labelled `name` lacks, to print
    /// under its row ([`lost_lines`]); none for a member the audit did not
    /// read.
    pub fn lost_lines_of(&self, name: &str) -> Vec<String> {
        self.members
            .iter()
            .find(|lines| lines.label == name)
            .map_or_else(Vec::new, |lines| {
                lost_lines((&lines.lost, &lines.word_step_skipped), LOST_LINE_INDENT)
            })
    }

    /// Each line of every member that the cut holds only by its words, to
    /// print after the rows ([`labelled_words_lines`]).
    pub fn words_lines(&self) -> Vec<String> {
        self.members.iter().flat_map(labelled_words_lines).collect()
    }

    /// Leave out of the drift each file that the composition gate after the
    /// audit judges: each file of `fixes`, the fixes by hand that the series
    /// holds, each at the files where its hold has not ended
    /// ([`adopted_fixes`]).
    ///
    /// A fix is no member's content, so no member tip explains its loss, and
    /// the gate, not the drift check, refuses a subject that lacks it at such
    /// a file, once, naming the file and its adopt. A subject that carries
    /// the fix there while the release it follows lacks it has no drift there
    /// either: the gate finds the file carried. A file that no fix holds stays
    /// in the drift.
    pub fn leave_fixes_to_gate(&mut self, fixes: &[AdoptedFix]) {
        self.unexplained.retain(|file| {
            !fixes
                .iter()
                .any(|fix| fix.files.iter().any(|held| held.path == *file))
        });
    }
}

/// What a cut audit measures its members and its drift from ([`audit_cut`]).
#[derive(Debug, Clone, Copy)]
pub struct AuditContext<'a> {
    /// The release a member's lost lines may be carried forward from: a
    /// cut's previous release. `None` carries nothing forward, so a member
    /// the subject lacks a line of is missing, however the subject came to
    /// drop it.
    pub previous: Option<&'a CommitId>,
    /// The upstream trunk's tip. Each member's own change is the diff from
    /// its base against it ([`Repo::change_base`], [`Repo::release_base`])
    /// to its tip (spec R6), so the trunk's commits after the member's fork
    /// point are never charged to the member.
    pub upstream_trunk: &'a CommitId,
    /// The parents of the subject that the trunk holds, each with where it
    /// came from: no member, but content the subject holds all the same.
    /// Only `release cut NAME --at` audits them, beside its members.
    pub trunk_parents: &'a [(String, CommitId)],
    /// What each of `trunk_parents` is measured from: the fork point of all
    /// the members and the trunk. A trunk parent's own fork point with the
    /// trunk is itself, so its own change would read carried whatever the
    /// subject holds. `None`, where there is no trunk parent, measures each
    /// from its own fork point, as a member.
    pub trunk_parent_base: Option<&'a CommitId>,
    /// The trunk commit whose files explain the drift beside each member's:
    /// a cut's audit base, the commit that the previous release and the
    /// trunk share ([`shared_base`]) or the members' fork point with the
    /// trunk. Only the drift check reads it.
    pub drift_trunk: &'a CommitId,
    /// What the drift is measured from, and what that commit is, which the
    /// row of a file no member explains names ([`unexplained_row`]). `None`
    /// runs no drift check, as for a first cut.
    pub drift_from: Option<(&'a CommitId, DriftBase)>,
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
/// parent set, keeping its recorded conflict resolutions, with `carriers`,
/// its parents that are earlier writes of its name, left out of what the
/// duplicate measures it against ([`jj::CutSpec::carriers`]). Adding a branch
/// or advancing the base therefore avoids replaying old conflicts. Dropping
/// a branch whose version a resolution by hand took can instead write that
/// file without another member's lines and with no conflict. The cut keeps
/// the duplicate, and its composition gate's hold on each fix by hand
/// ([`adopted_fixes`]) refuses such a candidate. `None` builds a fresh flat
/// merge. Both paths produce a flat octopus of exactly `request.parents`.
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

    /// The subject's parents, which [`Repo::release_base`] reads: the
    /// candidate's, which is no commit of the repository
    /// ([`jj::Candidate::parents`]), or the cut commit's. Every member of the
    /// audit is one of them, so for a member that the trunk contains they
    /// decide ancestry: its base is its tip. They never give such a member a
    /// base on the trunk's first-parent line.
    fn parents(&self, repo: &Repo) -> anyhow::Result<Vec<CommitId>> {
        match self {
            Self::Candidate(candidate) => Ok(candidate.parents().to_vec()),
            Self::Committed(cut) => Ok(repo.parent_commits(cut.as_str())?),
        }
    }

    /// What the subject holds of a member's own lines: the change from
    /// `fork_point` to `tip`, read path by path
    /// ([`jj::Repo::member_lines`]). `label` names the member in the reading.
    fn member_lines(
        &self,
        repo: &Repo,
        label: &str,
        (fork_point, tip): (&CommitId, &CommitId),
    ) -> anyhow::Result<MemberLines> {
        match self {
            Self::Candidate(candidate) => Ok(candidate.member_lines(label, (fork_point, tip))?),
            Self::Committed(cut) => Ok(repo.member_lines(label, (fork_point, tip), cut)?),
        }
    }

    /// The paths of the change `resolution` holds against `baseline` that
    /// the subject does not carry, each judged on its own
    /// ([`jj::Repo::uncarried_resolution_paths`]).
    fn uncarried_resolution_paths(
        &mut self,
        repo: &Repo,
        baseline: jj::ResolutionBaseline<'_>,
        resolution: &CommitId,
    ) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Candidate(candidate) => {
                Ok(candidate.uncarried_resolution_paths(baseline, resolution)?)
            }
            Self::Committed(cut) => Ok(repo.uncarried_resolution_paths(baseline, resolution, cut)?),
        }
    }

    /// Whether the subject holds what `other` holds at `path`, a
    /// repository-internal path: the same file, symlink, directory or
    /// conflict, or nothing at either.
    fn same_path_value(&self, repo: &Repo, other: &CommitId, path: &str) -> anyhow::Result<bool> {
        match self {
            Self::Candidate(candidate) => Ok(candidate.same_path_value(other, path)?),
            Self::Committed(cut) => Ok(repo.same_path_value(cut, other, path)?),
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
/// Each member is read by its own lines (spec R9): its own change is the
/// diff from its base by spec R6 to its tip, its fork point with
/// `context.upstream_trunk`, or, for a member the trunk contains, the base
/// that the subject's parents give it ([`Repo::release_base`]): a member is
/// a parent, so one the trunk contains is carried by ancestry. The cut
/// carries a member where it holds each line that the change adds and does
/// not hold again what it removes ([`jj::Repo::member_lines`]).
/// A member whose own path holds an unresolved conflict, in the cut or at
/// the member's tip, cannot be read there: it is inconclusive where it
/// lacks nothing else. A member that lacks a line fails the audit, unless
/// `context.previous` lacks that line too, read from the same base:
/// a member whose every lost line the previous release lacked is carried
/// forward ([`MemberLines::carry_forward`]). Each parent that the trunk
/// holds (`context.trunk_parents`) is read beside the members, from
/// `context.trunk_parent_base`.
///
/// The drift is what changed since `context.drift_from`, the release the
/// audited one follows, that no member, trunk parent or trunk explains:
/// merge-invented drift. That release is a cut's `context.previous`. It is
/// apart from `context.previous`, what a member's lost lines may be carried
/// from, because a release cut outside knives has a release it follows but
/// carries nothing forward: a member it lacks is missing. The series' first
/// record outside knives follows no release, and its drift is measured from
/// `context.drift_trunk`, its members' fork point with the trunk. With no
/// `drift_from` there is no drift check, as for a first cut. A caller that
/// holds fixes by hand after the audit leaves their files out of the drift
/// ([`CutAudit::leave_fixes_to_gate`]).
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
    let opened = Repo::open(repo)?;
    let parents = subject.parents(&opened)?;
    let mut audit = CutAudit::default();
    let read = members.iter().map(|member| (member, None)).chain(
        context
            .trunk_parents
            .iter()
            .map(|parent| (parent, context.trunk_parent_base)),
    );
    for ((name, tip), trunk_parent_base) in read {
        let base = match trunk_parent_base {
            Some(base) => base.clone(),
            None => opened.release_base(
                (tip, &opened.change_base(tip, context.upstream_trunk)?),
                &parents,
                || opened.taken_in(tip, context.upstream_trunk),
            )?,
        };
        let mut lines = subject.member_lines(&opened, name, (&base, tip))?;
        // A line the release held against already lacked is that release's
        // resolution, carried forward, not a loss: refusing it would make
        // every release with a hand-resolved conflict fail its own re-cut
        // forever. Whether that resolution is recorded is the caller's
        // question, not this audit's: a hand edit nothing recorded may have
        // dropped a member's line, so it is `knives release adopt` that
        // measures it before a cut.
        if lines.reading == Reading::Missing
            && let Some(previous) = context.previous
        {
            lines.carry_forward(&opened.member_lines(name, (&base, tip), previous)?);
        }
        match lines.reading {
            Reading::Carried => {}
            Reading::Inconclusive => audit.inconclusive.push(name.clone()),
            Reading::CarriedForward => audit.carried.push(name.clone()),
            Reading::Missing => audit.missing.push(name.clone()),
        }
        audit.members.push(lines);
    }
    if let Some((previous, _)) = context.drift_from {
        let drifted = subject.changed_files_since(repo, previous)?;
        let mut explained = BTreeSet::new();
        for (_, tip) in members.iter().chain(context.trunk_parents) {
            explained.extend(crate::jj::changed_files_between(
                repo,
                previous.as_str(),
                tip.as_str(),
            )?);
        }
        // The series' first record measures its drift from its trunk itself,
        // and a commit changes no file against itself.
        if context.drift_trunk != previous {
            explained.extend(crate::jj::changed_files_between(
                repo,
                previous.as_str(),
                context.drift_trunk.as_str(),
            )?);
        }
        audit.unexplained = drifted
            .into_iter()
            .filter(|file| !explained.contains(file))
            .collect();
    }
    Ok(audit)
}

/// How far each line that names what a member lacks is indented under the
/// member's row ([`lost_lines`]).
pub const LOST_LINE_INDENT: &str = "      ";

/// The lines that name what one member's reading against a release lacks,
/// each after `indent`.
///
/// `entries` are the reading's lost entries, and `word_step_skipped` the
/// ranges that the word step did not follow there.
///
/// There is one line for each lost entry that the release before did not
/// lack too, in the reading's order. A line that the member added and the
/// release does not hold, a hunk the member removed that the release holds
/// again, a path read whole, an executable bit and a path the reading cannot
/// read for its unresolved conflict are each named with their path; the
/// lines of a range that the word step did not follow are named once, by
/// the range.
///
/// This is the one renderer of a member's lacking lines, which `members
/// --verify`, the audit of a cut, the composition gate, `--carries` and
/// `--census` print under the row that names the member;
/// [`held_by_words_lines`] names the lines it holds only by their words.
/// Each text is read from the repository, so it is escaped.
pub fn lost_lines(
    (entries, word_step_skipped): (&[Lost], &[SkippedRange]),
    indent: &str,
) -> Vec<String> {
    use crate::ledger::inline_human_text;
    let mut named_ranges = Vec::new();
    let mut out = Vec::new();
    for lost in entries.iter().filter(|lost| !lost.carried_forward) {
        let path = inline_human_text(&lost.path);
        let text = lost
            .text
            .as_deref()
            .map_or_else(String::new, inline_human_text);
        let at = lost
            .line
            .map_or_else(String::new, |line| format!(":{line}"));
        let named = match lost.kind {
            LostKind::Added => format!("{path}{at}: added line lost ‹{text}›"),
            LostKind::Removed => format!("{path}{at}: removed lines put back ‹{text}›"),
            LostKind::File => format!("{path}: file differs from the member's ({text})"),
            LostKind::Mode => format!("{path}: mode differs from the member's ({text})"),
            LostKind::Conflict => format!("{path}: unresolved conflict {text}"),
            LostKind::WordLimit => {
                let skipped = word_step_skipped.iter().enumerate().find(|(_, range)| {
                    range.path == lost.path
                        && lost.line.is_some_and(|line| {
                            (range.first_line..=range.last_line).contains(&line)
                        })
                });
                if skipped.is_some_and(|(index, _)| named_ranges.contains(&index)) {
                    continue;
                }
                named_ranges.extend(skipped.map(|(index, _)| index));
                let span = skipped.map_or(at, |(_, range)| {
                    format!(":{}-{}", range.first_line, range.last_line)
                });
                format!(
                    "{path}{span}: the word step did not run (more than {} words); its lines \
                     count as lost",
                    crate::own_lines::WORD_LIMIT
                )
            }
        };
        out.push(format!("{indent}{named}"));
    }
    out
}

/// The lines that name each line of `lines`, one member's reading against a
/// release, that the release holds only by its words, each after `lead`.
///
/// Each names the member's line, and each line of the release's replaced
/// range that holds its words, so that a person sees the words another
/// change put around the member's own. Such a line does not make the member
/// read missing ([`lost_lines`]). Each text is read from the repository, so
/// it is escaped.
pub fn held_by_words_lines(lines: &MemberLines, lead: &str) -> Vec<String> {
    use crate::ledger::inline_human_text;
    use std::fmt::Write as _;
    lines
        .held_by_words
        .iter()
        .map(|held| {
            let mut named = format!(
                "{lead}{}:{} held only by its words ‹{}›",
                inline_human_text(&held.path),
                held.line,
                inline_human_text(&held.text)
            );
            for line in &held.release_lines {
                let _ = write!(
                    named,
                    "; release line {} ‹{}›",
                    line.line,
                    inline_human_text(&line.text)
                );
            }
            named
        })
        .collect()
}

/// [`held_by_words_lines`] after the member's label, as a report that names
/// several members prints them, after its rows.
pub fn labelled_words_lines(lines: &MemberLines) -> Vec<String> {
    let lead = format!("  {}: ", crate::ledger::inline_human_text(&lines.label));
    held_by_words_lines(lines, &lead)
}

/// Create and name a fresh cut for direct callers that do not need an audit.
/// A fresh merge replaces nothing, so it has no earlier write to hide.
pub fn cut(repo: &Path, request: &Cut, scheme: &ReleaseScheme) -> anyhow::Result<CommitId> {
    let candidate = candidate_cut(repo, request, None)?;
    Ok(publish_cut(candidate, &request.name, scheme, (&[], None))?.commit)
}

/// A member the composition gate's baseline records, as the gate names it.
#[derive(Debug, PartialEq, Eq)]
pub struct RecordedMember {
    /// The recorded commit. An unverified one is carried into the new cut
    /// event's evidence, so the next gate rechecks it.
    pub commit: CommitId,
    /// Rendered for a human: the branch still holding the commit when one does.
    pub name: String,
}

/// How a candidate cut relates to the recorded composition it is held
/// against.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompositionCheck {
    /// Recorded members whose own lines the candidate does not carry.
    pub dropped: Vec<RecordedMember>,
    /// Recorded members whose own path holds an unresolved conflict, in the
    /// candidate or at the member's tip, and which lack nothing else:
    /// unanswerable either way, exactly as the audit reports it.
    pub inconclusive: Vec<RecordedMember>,
    /// Recorded members carried by another commit with exactly their tree,
    /// reachable from a parent: the member rendered for a human, and the twin.
    pub twins: Vec<(String, CommitId)>,
    /// Whether this repository does not hold the newest recorded write
    /// ([`CompositionBaseline::newest_write`]). Nothing here can tell what
    /// that write carries, so the subject is refused as one that lacks it,
    /// as a recorded member this repository cannot resolve is.
    pub absent_write: bool,
    /// The fixes by hand that the series holds and the subject lacks at one
    /// of their files or more ([`CompositionDelta::fixes`]), each with the
    /// files it lacks. A file that the subject leaves conflicted is here too,
    /// unless the newest recorded write is conflicted there as well
    /// ([`Self::inconclusive_fixes`]). So is each fix whose files nothing
    /// here reads whole, with no file ([`FixLack::Unread`]): no subject
    /// carries it here.
    pub dropped_fixes: Vec<LackedFix>,
    /// The fixes by hand that the series holds at a file that the subject
    /// and the newest recorded write both leave conflicted, each with those files: unanswerable until the
    /// conflict is resolved, as for a member whose path is conflicted
    /// ([`Self::inconclusive`]). A knives cut then goes ahead with the
    /// conflict for the person to resolve in place and `adopt`; the write
    /// it makes is no reference of those files, so the fix stays held.
    pub inconclusive_fixes: Vec<LackedFix>,
    /// The lines of each recorded member that the check read against the
    /// subject, carried or not ([`jj::Repo::member_lines`]): what a refusal
    /// names under each member the subject lacks, and the lines it holds
    /// only by their words.
    pub members: Vec<MemberLines>,
}

impl CompositionCheck {
    /// Whether the check finds what refuses the subject unless
    /// `--allow-drop` lets it go ahead: a recorded member it lacks, a newest
    /// write that this repository does not hold, or a fix by hand it lacks
    /// at a file or whose files nothing here reads whole.
    pub const fn refuses(&self) -> bool {
        !self.dropped.is_empty() || self.absent_write || !self.dropped_fixes.is_empty()
    }

    /// Whether the check could not answer for a member or a fix by hand,
    /// as the subject leaves a conflict: no refusal, but no clean result.
    pub const fn is_inconclusive(&self) -> bool {
        !self.inconclusive.is_empty() || !self.inconclusive_fixes.is_empty()
    }

    /// The lines that name what the recorded member at `commit` lacks, to
    /// print under its row ([`lost_lines`]); none for a member the check
    /// did not read.
    pub fn lost_lines_of(&self, commit: &CommitId) -> Vec<String> {
        self.members
            .iter()
            .find(|lines| lines.commit == *commit)
            .map_or_else(Vec::new, |lines| {
                lost_lines((&lines.lost, &lines.word_step_skipped), LOST_LINE_INDENT)
            })
    }

    /// Each line of every recorded member that the subject holds only by
    /// its words, to print after the rows ([`labelled_words_lines`]).
    pub fn words_lines(&self) -> Vec<String> {
        self.members.iter().flat_map(labelled_words_lines).collect()
    }
}

/// A fix by hand that the series holds and the cut subject lacks, or
/// leaves inconclusive, at the files it names
/// ([`CompositionCheck::dropped_fixes`],
/// [`CompositionCheck::inconclusive_fixes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LackedFix {
    /// The release the adopt recorded.
    pub name: String,
    /// The adopt's commit.
    pub commit: CommitId,
    /// What the subject lacks of the fix.
    pub lacks: FixLack,
}

impl LackedFix {
    /// The files of the fix that the subject does not carry: none where
    /// nothing here reads them whole ([`FixLack::Unread`]).
    pub fn files(&self) -> &[HeldFile] {
        match &self.lacks {
            FixLack::Files { files, .. } => files,
            FixLack::Unread { .. } => &[],
        }
    }
}

/// What a cut subject lacks of a fix by hand ([`LackedFix`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixLack {
    /// The files of the fix that the subject does not carry
    /// ([`fix_lacks`]), each with the write it was held against, and why
    /// nothing here can measure the fix, where nothing can: the subject was
    /// then held to what each file's reference holds there.
    Files {
        files: Vec<HeldFile>,
        unmeasured: Option<Unmeasured>,
    },
    /// Nothing here reads the files of the fix whole: nothing here can
    /// measure the fix, as `unmeasured` says, and its event's text does
    /// not read whole, as `why` says ([`AdoptedFix::unread`]). Nothing here
    /// can tell whether the subject carries the fix.
    Unread {
        unmeasured: Unmeasured,
        why: crate::release_model::UnreadFixText,
    },
}

/// A file of a fix by hand that the series holds, and the recorded write
/// that it is held against ([`FixReferences::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldFile {
    /// The file, a repository path.
    pub path: String,
    /// What the file is held against: its reference, or why it has none.
    pub reference: FixReference,
    /// Whether the adopt's change against its baseline changes the file,
    /// where this repository measures the fix: a subject then carries the
    /// file where that change replays empty onto it there ([`fix_lacks`]).
    pub replayed: bool,
}

/// What a file of a fix by hand that the series holds is held against
/// ([`FixReferences::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixReference {
    /// The file's reference: the newest recorded write of the series that
    /// holds the file with no conflict, nothing there included. A write that
    /// leaves the file conflicted is no reference.
    Write(CommitId),
    /// No reference here: the write where the walk for the reference
    /// stopped, a recorded write of the release `name` that this repository
    /// does not hold. Each newer write leaves the file conflicted, and
    /// nothing here can read what this one holds there, so a fetch of it
    /// may bring the reference.
    Unread {
        /// The release that the write wrote.
        name: String,
        /// The write's commit.
        commit: CommitId,
    },
    /// No reference at all: every recorded write leaves the file conflicted.
    Conflicted,
}

/// A fix by hand that the series holds: what an `adopt` of one of its
/// releases recorded, at each file where its hold has not ended
/// ([`adopted_fixes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedFix {
    /// The release the adopt recorded.
    name: String,
    /// The adopt's commit.
    commit: CommitId,
    /// The files of the fix that the series holds, each on its own and with
    /// its reference: those that the adopt's change against its baseline
    /// changes and whose reference still holds that change
    /// ([`HeldFile::replayed`]), then those that its adopt event names
    /// beyond them ([`adopted_fix`]). Empty where nothing here reads them
    /// whole ([`Self::unread`]).
    files: Vec<HeldFile>,
    /// What the fix is measured against, or why nothing here can measure
    /// it, with why nothing here reads its files whole, where nothing does:
    /// its event, as an older knives writes it, names them in text that
    /// does not read whole
    /// ([`crate::release_model::AdoptedFixText::unread`]). The gate then
    /// refuses every subject, whatever it holds ([`fix_findings`]).
    measure: Result<FixBaseline, (Unmeasured, Option<crate::release_model::UnreadFixText>)>,
}

impl AdoptedFix {
    /// The baseline that the fix is the adopt's change against
    /// ([`FixBaseline::baseline`]), or `None` where nothing here can measure
    /// the fix ([`Self::unmeasured`]).
    fn baseline(&self) -> Option<jj::ResolutionBaseline<'_>> {
        self.measure.as_ref().ok().map(FixBaseline::baseline)
    }

    /// Why nothing here can measure the fix, or `None` where this
    /// repository measures it. The gate then holds each file of the fix to
    /// what its reference holds there ([`fix_lacks`]).
    const fn unmeasured(&self) -> Option<Unmeasured> {
        match &self.measure {
            Ok(_) => None,
            Err((why, _)) => Some(*why),
        }
    }

    /// Why nothing here can measure the fix and why nothing here reads its
    /// files whole, or `None` where this repository measures the fix or
    /// reads them whole.
    const fn unread(&self) -> Option<(Unmeasured, crate::release_model::UnreadFixText)> {
        match &self.measure {
            Err((unmeasured, Some(why))) => Some((*unmeasured, *why)),
            Ok(_) | Err((_, None)) => None,
        }
    }
}

/// Why nothing in this repository can measure a fix by hand
/// ([`FixLack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmeasured {
    /// This repository does not hold the adopt's commit.
    Absent,
}

/// What an adopted fix is measured against, as `adopt` measured it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FixBaseline {
    /// The write the adopt replaced and what that write composes, where it
    /// is the baseline ([`baseline_write`]).
    replaced: Option<(CommitId, Vec<CommitId>)>,
    /// What the adopt composes: its members
    /// ([`crate::release_model::ReleaseReading::members`]).
    composition: Vec<CommitId>,
}

impl FixBaseline {
    /// The replaced write rebuilt on the adopt's composition, or an
    /// automerge of that composition where the replaced write is not the
    /// baseline ([`resolution_baseline`]).
    fn baseline(&self) -> jj::ResolutionBaseline<'_> {
        resolution_baseline(
            self.replaced
                .as_ref()
                .map(|(write, composed)| (write, composed.as_slice())),
            &self.composition,
        )
    }
}

/// What `write`, the write that a resolution replaces, composes, where it
/// is the baseline that the resolution is measured against.
///
/// It is the baseline where this repository holds it and knives checked
/// the release it was written over: `event`, the ledger event that records
/// it, bears no `; unchecked:` mark
/// ([`crate::release_model::unchecked_mark`]). What it composes is read
/// with `writes`, the release's earlier writes ([`write_composition`]).
/// `None` where it is not the baseline, and the baseline is an automerge
/// instead ([`resolution_baseline`]). `adopt` measures a resolution so, and
/// the gate measures an adopted fix as `adopt` measured it
/// ([`fix_baseline`]).
pub fn baseline_write(
    repo: &Repo,
    (writes, (event, write)): (&EarlierWrites, (&Entry, &CommitId)),
) -> Result<Option<Vec<CommitId>>, jj::JjError> {
    if repo.resolve_commit(write.as_str()).is_err()
        || crate::release_model::unchecked_mark(event).is_some()
    {
        return Ok(None);
    }
    write_composition(repo, (writes, write)).map(Some)
}

/// The baseline of a resolution of a commit that composes `composition`.
///
/// It is `replaced`, the write that the resolution replaces and what that
/// write composes, where that write is the baseline ([`baseline_write`]),
/// rebuilt on `composition`; else an automerge of `composition`.
pub const fn resolution_baseline<'a>(
    replaced: Option<(&'a CommitId, &'a [CommitId])>,
    composition: &'a [CommitId],
) -> jj::ResolutionBaseline<'a> {
    match replaced {
        Some((write, composed)) => jj::ResolutionBaseline::Rebuilt {
            write,
            composed,
            members: composition,
        },
        None => jj::ResolutionBaseline::Automerge(composition),
    }
}

/// The fixes by hand that the series holds, release by release, from the
/// last name to the first, each release's newest adopt first.
///
/// There is one for each `adopt` that the ledger records of a release of
/// `entry`'s series, before or after that release's cut, whose hold has not
/// ended at one file or more. `writes` are the series' recorded writes,
/// newest first ([`crate::release_model::series_writes`]): the first is the
/// baseline's [`CompositionBaseline::newest_write`], the newest release's
/// newest recorded write. An adopt holds a fix when its event records one
/// ([`crate::release_model::adopted_fix_replaces`]).
///
/// Each file of a fix is held on its own, against its reference: the
/// newest of `writes` that holds the file with no conflict
/// ([`FixReferences::of`]). A write that leaves the file conflicted neither
/// ends its hold nor holds what a subject must hold there. The hold on a
/// file ends where its reference no longer holds the adopt's content there
/// ([`adopted_fix`]). So a fix that a later cut or edit kept stays held,
/// however many releases later, and a later write that removes the fix
/// from one file, or changes the fix's lines there again, ends the hold on
/// that file alone. Where this repository does not hold the newest write,
/// nothing here can tell what that write holds: no fix is held, and the
/// gate refuses for that write ([`CompositionCheck::absent_write`]).
pub fn adopted_fixes(
    repo: &Repo,
    entry: &RepoEntry,
    (ledger, tips): (&[Entry], &BookmarkTips),
    writes: &[SeriesWrite<'_>],
) -> anyhow::Result<Vec<AdoptedFix>> {
    if writes
        .first()
        .is_none_or(|newest| repo.resolve_commit(newest.commit.as_str()).is_err())
    {
        return Ok(Vec::new());
    }
    let mut references = FixReferences {
        writes,
        found: BTreeMap::new(),
    };
    let names = crate::release_model::recorded_release_names(ledger, &entry.release_scheme());
    let mut fixes = Vec::new();
    for name in names.into_iter().rev() {
        let events: Vec<(&Entry, RecordedWrite)> =
            crate::release_model::recorded_write_events(ledger, name).collect();
        if events
            .iter()
            .all(|(event, _)| crate::release_model::adopted_fix_replaces(event, name).is_none())
        {
            continue;
        }
        let published = tips.get(&BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(entry.publish_remote()),
        });
        let earlier = EarlierWrites::read(repo, name, ledger, published)?;
        for (at, (event, adopt)) in events.iter().enumerate() {
            let Some(replaced) = crate::release_model::adopted_fix_replaces(event, name) else {
                continue;
            };
            let replaced = events
                .iter()
                .skip(at + 1)
                .find(|(_, older)| older.commit.short() == replaced);
            let recorded = (*event, &adopt.commit);
            if let Some(fix) = adopted_fix(
                repo,
                &earlier,
                (name, recorded),
                (replaced, &mut references),
            )? {
                fixes.push(fix);
            }
        }
    }
    Ok(fixes)
}

/// The reference of each file of a fix by hand ([`HeldFile::reference`]),
/// found once for each file.
struct FixReferences<'a> {
    /// The series' recorded writes, newest first
    /// ([`crate::release_model::series_writes`]).
    writes: &'a [SeriesWrite<'a>],
    /// Each file found so far, with its reference.
    found: BTreeMap<String, FixReference>,
}

impl FixReferences<'_> {
    /// The reference of `file`, a repository path: the newest of
    /// [`Self::writes`] that holds it with no conflict, a file, a symlink, a
    /// directory or nothing. A knives write that leaves the file conflicted
    /// is no reference: it neither removes a fix there nor holds what a
    /// subject must hold. The walk stops at a write that this repository
    /// does not hold, which nothing here can read, and names it
    /// ([`FixReference::Unread`]).
    fn of(&mut self, repo: &Repo, file: &str) -> Result<FixReference, jj::JjError> {
        if let Some(found) = self.found.get(file) {
            return Ok(found.clone());
        }
        let mut reference = FixReference::Conflicted;
        for write in self.writes {
            if repo.resolve_commit(write.commit.as_str()).is_err() {
                reference = FixReference::Unread {
                    name: write.name.to_owned(),
                    commit: write.commit.clone(),
                };
                break;
            }
            if !repo.conflicted_at(&write.commit, file)? {
                reference = FixReference::Write(write.commit.clone());
                break;
            }
        }
        self.found.insert(file.to_owned(), reference.clone());
        Ok(reference)
    }
}

/// The fix by hand that `adopt`, a commit that `event`, an `adopt` event of
/// `name`, recorded, holds against `replaced`, the write that event
/// replaced, with its event, when the ledger still records it: each file of
/// it whose hold has not ended, with its reference, which `references`
/// finds. `None` where the hold has ended at every file. A fix whose files
/// nothing here reads whole is held at no file, and its hold never ends
/// here ([`AdoptedFix::unread`]).
///
/// Where this repository measures the fix, its files are those of the
/// adopt's change and those that `event` names beyond them
/// ([`measured_fix_files`]). A deletion is content: the file's absence is
/// the fix there.
///
/// An adopt that this repository does not hold is a fix that nothing here
/// can measure ([`Unmeasured`]). Its files are the ones that `event` names
/// ([`crate::release_model::adopted_fix_text`]), and each is held to what
/// its reference holds there. A reference that holds nothing at a file
/// holds the fix there as an absence, as nothing here can tell a fix that
/// deleted the file from a later write that removed it. That hold ends only
/// where a later reference holds the file again, and the file is then held
/// to what that reference holds. A file with no reference stays held, and
/// no subject carries it.
///
/// An event without its `fix_files` list, as an older knives writes it,
/// names its files in its text alone, which gives back a path that holds a
/// separator in pieces ([`crate::release_model::AdoptedFixFiles::from_text`]).
/// Where this repository measures the fix, the adopt's change gives its
/// real files, so a path that the text may name is a file of the fix only
/// where the adopt's own tree holds it: a piece such as `y.txt` of
/// `x, y.txt` is no file of it. Where nothing here can measure the fix, its
/// files are each path that the text may name, as no reading of the text
/// can tell them apart, and each is held as any other: one that its
/// reference does not hold is held as an absence. That may hold more than
/// the fix. Nothing here reads the fix's files whole where the text does
/// not read whole ([`crate::release_model::AdoptedFixText::unread`]): its
/// list of files resolved by hand is followed by another part, so the list
/// may end there or name a path that holds that part, or a list has more
/// than [`crate::release_model::TEXT_PATH_PIECES`] pieces, so it may name a
/// path of more pieces, which the text does not give. The fix is then held
/// at no file, no path of the text is read for it, and the gate refuses
/// every subject, naming the adopt and, where this repository does not hold
/// it and origin or the publish remote has a branch or tag at it, its fetch,
/// else `--allow-drop` alone (the binary's `release_cut::fix_remedy`). The
/// `fix_files` list of a newer event reads whole. A string that is no
/// repository path, as a peer's ledger may carry, is no file of the fix
/// ([`jj::is_repository_path`]).
fn adopted_fix(
    repo: &Repo,
    writes: &EarlierWrites,
    (name, (event, adopt)): (&str, (&Entry, &CommitId)),
    (replaced, references): (Option<&(&Entry, RecordedWrite)>, &mut FixReferences<'_>),
) -> Result<Option<AdoptedFix>, jj::JjError> {
    let text = crate::release_model::adopted_fix_text(event, name);
    let measure = fix_baseline(repo, writes, adopt, replaced)?.map_err(|why| (why, text.unread()));
    if matches!(measure, Err((_, Some(_)))) {
        return Ok(Some(AdoptedFix {
            name: name.to_owned(),
            commit: adopt.clone(),
            files: Vec::new(),
            measure,
        }));
    }
    let named = text.files();
    let from_text = named.from_text;
    let named: Vec<String> = named
        .files
        .into_iter()
        .filter(|file| jj::is_repository_path(file))
        .collect();
    let files = if let Ok(baseline) = &measure {
        let named = if from_text {
            repo.held_paths(adopt, &named)?
        } else {
            named
        };
        measured_fix_files(repo, (baseline.baseline(), adopt), named, references)?
    } else {
        let mut files = Vec::with_capacity(named.len());
        for path in named {
            let reference = references.of(repo, &path)?;
            files.push(HeldFile {
                path,
                reference,
                replayed: false,
            });
        }
        files
    };
    Ok((!files.is_empty()).then(|| AdoptedFix {
        name: name.to_owned(),
        commit: adopt.clone(),
        files,
        measure,
    }))
}

/// The files of the fix by hand of `adopt`, which this repository measures
/// against `baseline`, whose hold has not ended, each with its reference
/// ([`FixReferences::of`]) and whether the adopt's change changes it
/// ([`HeldFile::replayed`]).
///
/// The hold on a file of the adopt's change lasts while the change replays
/// empty onto the file's reference there
/// ([`jj::Repo::uncarried_resolution_paths`]), replayed once for each
/// reference. So the hold on a file that the fix deleted lasts while the
/// reference holds nothing there, and ends where it holds the file again.
/// A file of `named`, the files that the adopt's event names, each once,
/// that the change leaves out is one that nothing here measures: its hold
/// lasts while its reference holds there what the adopt holds, nothing at
/// both included. A file with no reference stays held.
fn measured_fix_files(
    repo: &Repo,
    (baseline, adopt): (jj::ResolutionBaseline<'_>, &CommitId),
    named: Vec<String>,
    references: &mut FixReferences<'_>,
) -> Result<Vec<HeldFile>, jj::JjError> {
    let changes = jj::resolution_changes(repo, baseline, adopt)?;
    let touched: Vec<String> = changes.resolved.into_iter().chain(changes.beyond).collect();
    let mut uncarried: Vec<(CommitId, Vec<String>)> = Vec::new();
    let mut files = Vec::new();
    for path in &touched {
        let reference = references.of(repo, path)?;
        let held = match &reference {
            FixReference::Unread { .. } | FixReference::Conflicted => true,
            FixReference::Write(write) => {
                if let Some((_, paths)) = uncarried.iter().find(|(onto, _)| onto == write) {
                    !paths.contains(path)
                } else {
                    let paths = repo.uncarried_resolution_paths(baseline, adopt, write)?;
                    let held = !paths.contains(path);
                    uncarried.push((write.clone(), paths));
                    held
                }
            }
        };
        if held {
            files.push(HeldFile {
                path: path.clone(),
                reference,
                replayed: true,
            });
        }
    }
    let touched: HashSet<&str> = touched.iter().map(String::as_str).collect();
    for path in named {
        if touched.contains(path.as_str()) {
            continue;
        }
        let reference = references.of(repo, &path)?;
        let held = match &reference {
            FixReference::Unread { .. } | FixReference::Conflicted => true,
            FixReference::Write(write) => repo.same_path_value(write, adopt, &path)?,
        };
        if held {
            files.push(HeldFile {
                path,
                reference,
                replayed: false,
            });
        }
    }
    Ok(files)
}

/// What the fix by hand of `adopt` is measured against, as `adopt`
/// measured it ([`adopted_fix`]): the write that the adopt replaced,
/// `replaced`, rebuilt on the adopt's composition, where that write is the
/// baseline ([`baseline_write`]); else an automerge of that composition.
/// Or why nothing here can measure it: this repository does not hold the
/// adopt.
fn fix_baseline(
    repo: &Repo,
    writes: &EarlierWrites,
    adopt: &CommitId,
    replaced: Option<&(&Entry, RecordedWrite)>,
) -> Result<Result<FixBaseline, Unmeasured>, jj::JjError> {
    if repo.resolve_commit(adopt.as_str()).is_err() {
        return Ok(Err(Unmeasured::Absent));
    }
    let reading = read_release(repo, (writes, adopt))?;
    let replaced = match replaced {
        Some((event, older)) => baseline_write(repo, (writes, (*event, &older.commit)))?
            .map(|composed| (older.commit.clone(), composed)),
        None => None,
    };
    Ok(Ok(FixBaseline {
        replaced,
        composition: reading.members,
    }))
}

/// What the composition gate compares: the cut's parents against the members
/// its baseline records, the previous cut's and the newest write's.
#[derive(Debug)]
pub struct CompositionDelta<'a> {
    pub baseline: &'a CompositionBaseline,
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
    /// The fixes by hand that the series holds ([`adopted_fixes`]): each
    /// adopt's fix, of any release of the series, at each file where its
    /// hold has not ended, with the file's reference. The subject must
    /// carry each such file ([`fix_lacks`]).
    pub fixes: &'a [AdoptedFix],
}

/// Members the baseline records that the cut subject does not carry, and the
/// fixes by hand that the series holds that it lacks.
///
/// Identity and ancestry account for a member that is still a parent, was
/// advanced past, or entered through the cut's base. A member whose change was
/// rewritten — `jj rebase` keeps the change id and mints a new commit — is
/// carried when a rewrite that stands for it is reachable from a parent or
/// the base: a successor of it, or a commit with its tree
/// ([`Repo::standing_rewrites_of`]). The recorded commit itself is never an
/// ancestor again after a rebase. A commit that only shares its change id,
/// such as the older version a newer member replaced, carries nothing for
/// it here, and the checks below measure the member. The one exception is
/// the succession the ledger records: a member of the previous cut whose
/// change id a member of the newest write has is superseded by that member,
/// as `advance` recorded it, and is not held; that member is. A member
/// recreated outside jj with exactly its tree is carried when that twin is
/// reachable
/// from a parent past the base ([`Repo::same_tree_between`]), and the twin
/// is named. A member that none of these account for is read by its own
/// lines against the subject ([`jj::Repo::member_lines`]), the same measure
/// [`audit_cut`] applies to current members, taken from the member's own
/// fork point so a moved base is never charged to the member: that accounts
/// for one that landed upstream as a squash, or one rewritten in another
/// clone. A member whose own path the subject leaves conflicted, or that is
/// conflicted at the member's tip, and that lacks nothing else, is
/// inconclusive. A member the trunk reaches but the cut subject does not is
/// dropped without a reading, as at knives 5.1.0 (spec R6): the trunk
/// carries it, and this subject does not. Every other member is read from
/// its fork point with the trunk ([`Repo::change_base`]). A recorded
/// commit this repository cannot resolve counts as dropped: unverifiable
/// must not read as carried. So does the newest write itself: a subject
/// held against a write this repository does not hold is refused
/// ([`CompositionCheck::absent_write`]), never held to less.
///
/// Each fix by hand that the series holds ([`CompositionDelta::fixes`]) is
/// held file by file, each file against its reference ([`fix_lacks`]),
/// whatever the subject's ancestry. A fix the subject lacks at a file is
/// named by its adopt, with each file it lacks
/// ([`CompositionCheck::dropped_fixes`]); at a file that the subject leaves
/// conflicted, where the newest recorded write is conflicted there too, as
/// a knives cut over a conflicted write is, the fix is inconclusive instead
/// and refuses nothing ([`CompositionCheck::inconclusive_fixes`]), so the
/// cut goes ahead with the conflict for the person to resolve in place and
/// `adopt`. A file the subject leaves conflicted where the newest write is
/// not, as on a bookmark left behind a later adopt, is lacked. A fix whose
/// files nothing here reads whole is lacked whatever the subject holds
/// ([`AdoptedFix::unread`]).
pub fn uncarried_recorded_members(
    repo: &Repo,
    path: &Path,
    subject: &mut CutSubject<'_>,
    delta: &CompositionDelta<'_>,
) -> anyhow::Result<CompositionCheck> {
    let mut check = CompositionCheck::default();
    let conflicted = subject.conflicted_files(path)?;
    if let Some(write) = &delta.baseline.newest_write {
        check.absent_write = repo.resolve_commit(write.commit.as_str()).is_err();
        // The series holds no fix where this repository does not hold that
        // write ([`adopted_fixes`]).
        for fix in delta.fixes {
            fix_findings(
                (repo, &mut *subject),
                fix,
                (&write.commit, &conflicted),
                &mut check,
            )?;
        }
    }
    // The newest write's members, by change id where this repository holds
    // them: what a member of the previous cut is superseded by.
    let newest = delta
        .baseline
        .newest_write
        .as_ref()
        .map_or(&[][..], |write| write.members.as_slice());
    let mut successors = Vec::new();
    for member in newest {
        if repo.resolve_commit(member.as_str()).is_ok() {
            successors.push(repo.change_id_of(member.as_str())?);
        }
    }
    for member in delta.baseline.members() {
        if delta.parents.contains(member) {
            continue;
        }
        if repo.resolve_commit(member.as_str()).is_err() {
            check.dropped.push(RecordedMember {
                commit: member.clone(),
                name: format!("{} (not known to this repository)", member.short()),
            });
            continue;
        }
        if !newest.contains(member) && successors.contains(&repo.change_id_of(member.as_str())?) {
            continue;
        }
        if reaches_any(repo, member, delta)? {
            continue;
        }
        let rewrites = repo.standing_rewrites_of(member)?;
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
            check.dropped.push(recorded_member(member, delta.tips));
            continue;
        }
        // The guard leaves a member that the trunk does not contain, whose
        // base is its fork point with the trunk.
        let change_base = repo.change_base(member, delta.trunk)?;
        let base = repo.release_base((member, &change_base), delta.parents, || {
            repo.taken_in(member, delta.trunk)
        })?;
        let recorded = recorded_member(member, delta.tips);
        let lines = subject.member_lines(repo, &recorded.name, (&base, member))?;
        match lines.reading {
            Reading::Carried | Reading::CarriedForward => {}
            Reading::Inconclusive => check.inconclusive.push(recorded),
            Reading::Missing => check.dropped.push(recorded),
        }
        check.members.push(lines);
    }
    Ok(check)
}

/// Record what the subject lacks of `fix`, one of the fixes by hand that the
/// series holds ([`fix_lacks`]), in `check`: a file that the subject leaves
/// conflicted, of `conflicted`, where `newest`, the newest recorded write,
/// holds that conflict too, as inconclusive
/// ([`CompositionCheck::inconclusive_fixes`]): the conflict came from a
/// recorded write, and the person resolves it in place. Every other file
/// it lacks as dropped ([`CompositionCheck::dropped_fixes`]), refused with
/// the move to the newest write. A fix whose files nothing here reads whole
/// ([`AdoptedFix::unread`]) is dropped with no file, whatever the subject
/// holds: nothing here can tell whether the subject carries it, and the
/// gate never holds a subject to less than the fix.
fn fix_findings(
    (repo, subject): (&Repo, &mut CutSubject<'_>),
    fix: &AdoptedFix,
    (newest, conflicted): (&CommitId, &[String]),
    check: &mut CompositionCheck,
) -> anyhow::Result<()> {
    if let Some((unmeasured, why)) = fix.unread() {
        check.dropped_fixes.push(LackedFix {
            name: fix.name.clone(),
            commit: fix.commit.clone(),
            lacks: FixLack::Unread { unmeasured, why },
        });
        return Ok(());
    }
    let (mut open, mut lacked) = (Vec::new(), Vec::new());
    for file in fix_lacks(repo, subject, fix)? {
        if conflicted.contains(&file.path) && repo.conflicted_at(newest, &file.path)? {
            open.push(file);
        } else {
            lacked.push(file);
        }
    }
    for (files, found) in [
        (lacked, &mut check.dropped_fixes),
        (open, &mut check.inconclusive_fixes),
    ] {
        if !files.is_empty() {
            found.push(LackedFix {
                name: fix.name.clone(),
                commit: fix.commit.clone(),
                lacks: FixLack::Files {
                    files,
                    unmeasured: fix.unmeasured(),
                },
            });
        }
    }
    Ok(())
}

/// The files of `fix`, one of the fixes by hand that the series holds
/// ([`AdoptedFix`]), that the cut subject does not carry, each judged on its
/// own against its reference ([`HeldFile::reference`]).
///
/// The subject carries a file that the adopt's change changes where that
/// change replays empty onto it there ([`HeldFile::replayed`]): a member
/// that changes other lines of a file whose lines the fix changed leaves
/// the fix there. Otherwise it carries a file where it holds exactly what
/// the file's reference holds there, nothing at both included: a later
/// write may change a file of the fix, a later adopt that refines it or an
/// `include` of a member that touches it, and the replay then conflicts,
/// as it does for any other content at a file the fix added. A file that
/// nothing here measures is held by that test alone, and a file with no
/// reference by the replay alone. So a subject that holds a file that the
/// fix deleted lacks the fix there, and so does one that leaves the file
/// conflicted; the caller reports that file as inconclusive where the
/// newest recorded write is conflicted there too ([`fix_findings`]).
///
/// Only the subject's own tree counts: the candidate a cut builds, or the
/// commit a record names. A subject with the adopt on its first-parent line
/// carries nothing for it, as its own merge may undo the fix.
fn fix_lacks(
    repo: &Repo,
    subject: &mut CutSubject<'_>,
    fix: &AdoptedFix,
) -> anyhow::Result<Vec<HeldFile>> {
    let uncarried = match fix.baseline() {
        Some(baseline) if fix.files.iter().any(|file| file.replayed) => {
            subject.uncarried_resolution_paths(repo, baseline, &fix.commit)?
        }
        _ => Vec::new(),
    };
    let mut lacked = Vec::new();
    for file in &fix.files {
        if file.replayed && !uncarried.contains(&file.path) {
            continue;
        }
        let held = match &file.reference {
            FixReference::Write(reference) => {
                subject.same_path_value(repo, reference, &file.path)?
            }
            FixReference::Unread { .. } | FixReference::Conflicted => false,
        };
        if !held {
            lacked.push(file.clone());
        }
    }
    Ok(lacked)
}

/// `commit` as the gate names it ([`recorded_member_name`]).
fn recorded_member(commit: &CommitId, tips: &BookmarkTips) -> RecordedMember {
    RecordedMember {
        commit: commit.clone(),
        name: recorded_member_name(commit, tips),
    }
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
            fix_files: Vec::new(),
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
/// first line of every block, in the order written: the union never
/// renumbers, so two blocks claiming one version read as two such lines, and
/// settling that is a person's. Where the blocks at a place are not plain
/// `### ` lists in a file's one `[Unreleased]` section, the union writes each
/// of them whole, and one line for the file names each `### ` heading that
/// two of them leave in one section, for a person to join.
///
/// Every other conflict stays a person's. Independent branches that each
/// append a config key land in the same regions, so a real cut carries real
/// conflicts: one ten-parent cut had a four-sided conflict in one file and a
/// three-sided one in another. Resolving those correctly is a semantic
/// judgement about the config, which a tool cannot make. Saying exactly where
/// they are, and what shape the resolution takes, is the part a tool can do.
/// An edit reports the same way. It keeps a resolution only where the same
/// conflict comes back, so what is left is the conflict the edit itself
/// brought in, or the plain merge's conflict where it wrote that merge. A
/// resolution by hand is a change knives did not write, and every write over
/// the release refuses until `knives release adopt <release>` records it, so
/// the guidance names that step for `release`.
pub fn conflict_guidance(release: &str, files: &[String], unions: &[jj::UnionedFile]) -> String {
    use crate::ledger::inline_human_text;
    let mut lines = union_guidance(unions);
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
    // The release is a name read from the repository, inside a command.
    lines.push(format!(
        "  after resolving them by hand, run `knives release adopt {}` before the next write, \
         which refuses until then",
        crate::ledger::shell_word(release)
    ));
    lines.join("\n")
}

/// The lines of [`conflict_guidance`] that name each release log written as
/// the union of its sides, with each block's first line; none where the
/// write unioned no log.
fn union_guidance(unions: &[jj::UnionedFile]) -> Vec<String> {
    use crate::ledger::inline_human_text;
    let mut lines = Vec::new();
    if !unions.is_empty() {
        lines.push(format!(
            "  {} release log(s) written as the union of their sides, each block named by its \
             first line, in this order:",
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
        // A path read from the tree, and each heading as a member's file
        // holds it.
        for union in unions.iter().filter(|union| !union.left_twice.is_empty()) {
            let headings: Vec<String> = union
                .left_twice
                .iter()
                .map(|heading| format!("`{}`", inline_human_text(heading)))
                .collect();
            lines.push(format!(
                "  {}: left {} twice: knives joins only plain `### ` lists in a file's one \
                 [Unreleased] section; join them by hand",
                inline_human_text(&union.path),
                headings.join(", ")
            ));
        }
    }
    lines
}

/// Run the steps of a command that writes the release, then print the
/// repeated `### ` headings of its last write once (README.md, "Release workflow").
///
/// `steps` gets the slot for the command's last write ([`ReportedWrite`]).
/// A step that prints a write's report with its headings takes the write out
/// of the slot. After the steps, whether they end or fail, the write still
/// in the slot prints its headings, and the steps' result is returned as it
/// is.
pub fn with_last_write_headings<T>(
    steps: impl FnOnce(&mut Option<ReportedWrite>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut last = None;
    let finished = steps(&mut last);
    if let Some(last) = &last {
        last.report_headings();
    }
    finished
}

/// What a cut prints about the tree it wrote.
///
/// That tree is `written`'s, written under `release` with `unions` and read
/// through the one opening made after the write ([`ReportedWrite`]): the
/// conflict report ([`conflict_report`]), then a line for each `### `
/// heading that a release log of the tree holds more than once in its one
/// `[Unreleased]` section ([`report_repeated_headings`]). The cut takes its
/// write out of the slot of [`with_last_write_headings`] to print it, so
/// those headings print here and not again.
pub fn written_report(
    written: &ReportedWrite,
    release: &str,
    unions: &[jj::UnionedFile],
) -> String {
    let mut report = conflict_report(release, written.conflicts(), unions);
    for line in written.heading_lines() {
        report.push('\n');
        report.push_str(&line);
    }
    report
}

/// A release a command wrote, read through one opening of the repository as
/// soon as the write is done.
///
/// The opening lists the files its tree leaves conflicted when they are
/// asked for, which the write's own report prints ([`conflict_report`]),
/// and reads its repeated `### ` headings ([`with_last_write_headings`]).
/// Each release write is read through it as soon as it is written; when
/// those headings print is README.md's rule ("Release workflow").
#[derive(Debug)]
pub struct ReportedWrite {
    /// The opening and the commit read through it, or the error that the
    /// opening, or the lookup of a moved release's commit through it, gave.
    reading: Result<(Repo, CommitId), jj::JjError>,
    /// The files the tree leaves conflicted, listed through the opening
    /// the first time they are asked for, or the error jj gave.
    conflicts: std::cell::OnceCell<Result<Vec<String>, jj::JjError>>,
}

impl ReportedWrite {
    /// Open the repository at `path` once, after the write of `commit`.
    pub fn read(path: &Path, commit: CommitId) -> Self {
        Self {
            reading: Repo::open(path).map(|opened| (opened, commit)),
            conflicts: std::cell::OnceCell::new(),
        }
    }

    /// Open the repository at `path` once, after a move that rewrote the
    /// release under `change`, and find through that opening the one
    /// visible commit of `change`: the release the move left. The lookup
    /// reads no name, so a release name that a concurrent operation left
    /// conflicted does not stop it. No visible commit of `change`, or more
    /// than one, is an error that the reading keeps, as it keeps the
    /// opening's.
    pub fn read_moved(path: &Path, change: &ChangeId) -> Self {
        Self {
            reading: Repo::open(path).and_then(|opened| {
                let visible = opened.visible_commits_of_change(change.as_str())?;
                let [commit] = <[CommitId; 1]>::try_from(visible).map_err(|visible| {
                    jj::JjError::Revision {
                        revision: change.as_str().to_owned(),
                        detail: format!(
                            "the move left {} visible commits of the release's change, not one",
                            visible.len()
                        ),
                    }
                })?;
                Ok((opened, commit))
            }),
            conflicts: std::cell::OnceCell::new(),
        }
    }

    /// The repository as the write left it, or the error its reading gave.
    pub fn opened(&self) -> Result<&Repo, &jj::JjError> {
        self.reading.as_ref().map(|(opened, _)| opened)
    }

    /// The files the tree leaves conflicted, or the error jj gave.
    pub fn conflicts(&self) -> Result<&[String], &jj::JjError> {
        let (opened, commit) = self.reading.as_ref()?;
        self.conflicts
            .get_or_init(|| opened.conflicted_files(commit.as_str()))
            .as_deref()
    }

    /// Print a line for each `### ` heading that a release log of the tree
    /// holds more than once in its one `[Unreleased]` section, read through
    /// the same opening.
    fn report_headings(&self) {
        for line in self.heading_lines() {
            println!("{line}");
        }
    }

    /// The lines [`Self::report_headings`] prints ([`heading_report`]).
    fn heading_lines(&self) -> Vec<String> {
        match &self.reading {
            Ok((opened, commit)) => {
                heading_report(opened.repeated_headings(commit.as_str()).as_deref())
            }
            Err(error) => heading_report(Err(error)),
        }
    }
}

/// The lines a release write prints about the files its tree leaves
/// conflicted.
///
/// They are `conflicts` as jj read them, in [`conflict_guidance`]'s words,
/// or the error jj gave. The error is escaped: it quotes jj, the store and
/// the paths in the tree.
pub fn conflict_report(
    release: &str,
    conflicts: Result<&[String], &jj::JjError>,
    unions: &[jj::UnionedFile],
) -> String {
    match conflicts {
        Ok(files) => conflict_guidance(release, files, unions),
        Err(error) => format!(
            "  could not list conflicts: {}",
            crate::ledger::problem_human_text(&error.to_string())
        ),
    }
}

/// A member that a rebase moved and left conflicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedMember {
    /// Its name in the release's description ([`parent_sources`]): its
    /// branch, or its short commit id where no bookmark holds it.
    pub name: String,
    /// The files its rebased tip leaves conflicted.
    pub files: Vec<String>,
}

/// [`conflict_report`] for a write of `knives release rebase`: the rebase's
/// own, or the write of the landed-member drop that follows it.
///
/// After the release's own lines comes one line for each of `members`, the
/// members the rebase moved and left conflicted, with the files of each.
/// `no conflicts in this release`, and its union form, say that nothing is
/// left to resolve. So where a member is conflicted and the release commit
/// is not, the release's line says only that the release commit holds no
/// conflicted file. A member's name and files are read from the repository,
/// so they reach the terminal escaped.
pub fn rebase_conflict_report(
    (repo, release): (&RepoName, &str),
    conflicts: Result<&[String], &jj::JjError>,
    unions: &[jj::UnionedFile],
    members: &[ConflictedMember],
) -> String {
    use crate::ledger::inline_human_text;
    let mut lines = match conflicts {
        Ok([]) if !members.is_empty() => {
            let mut lines = union_guidance(unions);
            lines.push(format!("{repo}: no conflicted file in the release commit"));
            lines
        }
        _ => vec![conflict_report(release, conflicts, unions)],
    };
    for member in members {
        let files: Vec<String> = member
            .files
            .iter()
            .map(|file| inline_human_text(file))
            .collect();
        lines.push(format!(
            "{repo}: member {} holds {} conflicted file(s) after the rebase: {}; resolve them \
             on its branch",
            inline_human_text(&member.name),
            member.files.len(),
            files.join(", ")
        ));
    }
    lines.join("\n")
}

/// Print one line for each `### ` heading that a release log of `commit`'s
/// tree holds more than once in its one `[Unreleased]` section.
///
/// `adopt` and the record of a published composition call it after they
/// recorded that tree ([`jj::repeated_headings`]); a command that writes
/// the release prints the same lines for its last write
/// ([`with_last_write_headings`]). Every release log in the tree is read,
/// changed or not, and whoever placed its lines: a clean merge, a member or
/// a person. A heading that the union's own line names as left twice inside
/// that one `[Unreleased]` section is named here too. knives changes no line
/// for it: a person joins the lists before the release is pushed.
pub fn report_repeated_headings(path: &Path, commit: &CommitId) {
    for line in heading_report(jj::repeated_headings(path, commit.as_str()).as_deref()) {
        println!("{line}");
    }
}

/// The lines [`report_repeated_headings`] prints: those of `repeated` as jj
/// read them ([`repeated_heading_lines`]), or the error jj gave, escaped as
/// [`conflict_report`] escapes it.
fn heading_report(repeated: Result<&[jj::RepeatedHeadings], &jj::JjError>) -> Vec<String> {
    match repeated {
        Ok(repeated) => repeated_heading_lines(repeated),
        Err(error) => vec![format!(
            "  could not read the release logs for repeated headings: {}",
            crate::ledger::problem_human_text(&error.to_string())
        )],
    }
}

/// The lines [`report_repeated_headings`] prints, one for each file and
/// heading. The path is read from the tree and the heading from the file, so
/// each reaches the terminal escaped.
fn repeated_heading_lines(repeated: &[jj::RepeatedHeadings]) -> Vec<String> {
    use crate::ledger::inline_human_text;
    repeated
        .iter()
        .flat_map(|file| {
            file.headings.iter().map(|(heading, count)| {
                let times = if *count == 2 {
                    "twice".to_owned()
                } else {
                    format!("{count} times")
                };
                format!(
                    "  {}: [Unreleased] holds `{}` {times}; knives changed no line for it; join \
                     them by hand before you push",
                    inline_human_text(&file.path),
                    inline_human_text(heading)
                )
            })
        })
        .collect()
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

    #[test]
    fn a_heading_left_twice_is_named_with_its_file_and_escaped() {
        // Given: a release log that a write unioned whole, which leaves in two
        // blocks of one section a heading with a control character, and a
        // release log whose lists the write joined.
        let left = jj::UnionedFile {
            path: "CHANGELOG.md".to_owned(),
            first_lines: vec!["### Fixed\u{1b}[2J".to_owned(); 2],
            left_twice: vec!["### Fixed\u{1b}[2J".to_owned()],
        };
        let joined = jj::UnionedFile {
            path: "RELEASE_NOTES.md".to_owned(),
            first_lines: vec!["### Fixed".to_owned()],
            left_twice: Vec::new(),
        };

        // When: its guidance is rendered.
        let rendered = conflict_guidance("release/2026-08-30", &[], &[left, joined]);

        // Then: one line names the first file and its heading, escaped, and
        // no line names the joined file so.
        assert!(
            rendered.contains(
                "\n  CHANGELOG.md: left `### Fixed\u{fffd}[2J` twice: knives joins only plain \
                 `### ` lists in a file's one [Unreleased] section; join them by hand\n"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains('\u{1b}'), "{rendered}");
        assert_eq!(rendered.matches(" twice: ").count(), 1, "{rendered}");
    }

    #[test]
    fn a_repeated_heading_is_named_once_for_each_file_and_heading_escaped() {
        // Given: a changelog whose `[Unreleased]` section holds `### Added`
        // twice and a heading with a control character three times, and
        // release notes that hold `### Fixed` twice.
        let repeated = [
            jj::RepeatedHeadings {
                path: "CHANGELOG.md".to_owned(),
                headings: vec![
                    ("### Added".to_owned(), 2),
                    ("### Fixed\u{1b}[2J".to_owned(), 3),
                ],
            },
            jj::RepeatedHeadings {
                path: "docs/RELEASE_NOTES.md".to_owned(),
                headings: vec![("### Fixed".to_owned(), 2)],
            },
        ];

        // When: its lines are rendered.
        let lines = repeated_heading_lines(&repeated);

        // Then: one line names each file and heading, with how many times,
        // escaped.
        assert_eq!(
            lines,
            [
                "  CHANGELOG.md: [Unreleased] holds `### Added` twice; knives changed no line for \
                 it; join them by hand before you push",
                "  CHANGELOG.md: [Unreleased] holds `### Fixed\u{fffd}[2J` 3 times; knives changed \
                 no line for it; join them by hand before you push",
                "  docs/RELEASE_NOTES.md: [Unreleased] holds `### Fixed` twice; knives changed no \
                 line for it; join them by hand before you push",
            ]
        );
    }

    #[test]
    fn a_jj_error_in_the_conflict_report_reaches_the_terminal_escaped() {
        // Given: an error jj gave, whose text holds an escape sequence and a
        // bell, as the store's text or a path in the tree may.
        let error = jj::JjError::Revision {
            revision: "release/2026-08-30".to_owned(),
            detail: "no such commit\u{1b}[2J\u{7}".to_owned(),
        };

        // When: the conflict report and the repeated-heading lines name it.
        let conflicts = conflict_report("release/2026-08-30", Err(&error), &[]);
        let headings = heading_report(Err(&error));

        // Then: each names the error with its control characters escaped.
        let shown =
            "could not resolve revision `release/2026-08-30`: no such commit\u{fffd}[2J\u{fffd}";
        assert_eq!(conflicts, format!("  could not list conflicts: {shown}"));
        assert_eq!(
            headings,
            [format!(
                "  could not read the release logs for repeated headings: {shown}"
            )]
        );
    }

    #[test]
    fn a_rebase_with_no_conflicted_member_reports_as_every_write_does() {
        // Given: a rebased release commit, clean or conflicted, and no
        // conflicted member.
        let repo = RepoName::new("demo");
        let files = ["shared.txt".to_owned()];
        for conflicts in [&[][..], &files[..]] {
            // When: the rebase's report is rendered.
            let rendered =
                rebase_conflict_report((&repo, "release/2026-08-30"), Ok(conflicts), &[], &[]);

            // Then: it is the conflict report of any other write.
            assert_eq!(
                rendered,
                conflict_report("release/2026-08-30", Ok(conflicts), &[])
            );
        }
    }

    #[test]
    fn a_conflicted_member_is_named_and_the_release_no_longer_says_no_conflicts() {
        // Given: a clean release commit, one release log unioned, and one
        // moved member whose name and file hold control characters.
        let repo = RepoName::new("demo");
        let union = jj::UnionedFile {
            path: "CHANGELOG.md".to_owned(),
            first_lines: vec!["### Fixed".to_owned()],
            left_twice: Vec::new(),
        };
        let member = ConflictedMember {
            name: "feat/d\u{1b}[2J".to_owned(),
            files: vec!["f".to_owned(), "a\u{7}b".to_owned()],
        };

        // When: the rebase's report is rendered, with and without the union.
        let plain = rebase_conflict_report(
            (&repo, "release/2026-08-30"),
            Ok(&[]),
            &[],
            std::slice::from_ref(&member),
        );
        let unioned = rebase_conflict_report(
            (&repo, "release/2026-08-30"),
            Ok(&[]),
            std::slice::from_ref(&union),
            &[member],
        );

        // Then: the release's line says only that its commit holds no
        // conflicted file, and the member's line names it and its files,
        // escaped.
        let member_line = "demo: member feat/d\u{fffd}[2J holds 2 conflicted file(s) after the \
                           rebase: f, a\u{fffd}b; resolve them on its branch";
        assert_eq!(
            plain,
            format!("demo: no conflicted file in the release commit\n{member_line}")
        );
        assert!(
            unioned.starts_with("  1 release log(s) written as the union of their sides"),
            "{unioned}"
        );
        assert!(
            unioned.ends_with(&format!(
                "\ndemo: no conflicted file in the release commit\n{member_line}"
            )),
            "{unioned}"
        );
        assert!(!unioned.contains("conflicts in this release"), "{unioned}");
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
            members: Vec::new(),
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
                members: Vec::new(),
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
        let ledger = [cut_event(name, written.as_str(), &[member.as_str()])];
        let standing = |ledger: &[Entry], commit: &CommitId| {
            let writes = EarlierWrites::read(&opened, name, ledger, None).expect("read writes");
            in_hand_write(&InHand {
                repo: &opened,
                ledger,
                name,
                commit,
                writes: &writes,
                published: None,
            })
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
                write: recorded_cut(&written),
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
