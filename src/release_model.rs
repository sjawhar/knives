//! Release-domain rules shared by command verbs and reports.
//!
//! This module owns facts about release names, recorded cuts, and consumer pins.
//! Commands gather their I/O and render their answers around these rules.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, Finding, FindingKind, Subject};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, is_our_release, is_release_name,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, FixFile, Kind, RecordedParent};
use crate::pins::{Pin, scan};
/// Scan evidence for one consumer's fetched pin texts.
#[derive(Debug, Default)]
pub struct ConsumerScan {
    pub pins: Vec<Pin>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

impl ConsumerScan {
    pub fn extend(&mut self, other: Self) {
        self.pins.extend(other.pins);
        self.notes.extend(other.notes);
        self.problems.extend(other.problems);
    }
}

/// Classify already-fetched consumer pin texts for one repository's release scheme.
///
/// The caller owns where texts came from (a forge, cache, checkout ref, or working
/// copy); this release-domain function only parses and scopes their pin evidence.
pub fn scan_consumer_texts<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    slug: Option<&str>,
    scheme: &ReleaseScheme,
) -> ConsumerScan {
    let mut result = ConsumerScan::default();
    let scope = ConsumerPinScope { slug, scheme };
    extend_scanned_texts(&mut result, files, &scope);
    result
}

struct ConsumerPinScope<'a> {
    slug: Option<&'a str>,
    scheme: &'a ReleaseScheme,
}

fn extend_scanned_texts<'a>(
    result: &mut ConsumerScan,
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    scope: &ConsumerPinScope<'_>,
) {
    for (file, text) in files {
        let parsed = scan(file, text, scope.scheme);
        result.pins.extend(
            parsed
                .pins
                .into_iter()
                .filter(|pin| scope.slug.is_none_or(|slug| pin.source.contains(slug))),
        );
        result.problems.extend(
            parsed
                .problems
                .into_iter()
                .filter(|problem| scope.slug.is_none_or(|slug| problem.source.contains(slug)))
                .map(|problem| problem.to_string()),
        );
    }
}

/// The repository's name as it appears in a dependency line, e.g. `sandbox-runner`.
pub fn repo_slug(entry: &RepoEntry) -> Option<String> {
    crate::remote_url::repository_name(entry.remote(Role::Origin)).map(str::to_owned)
}

/// The release the next cut carries: the local composition in hand, preferred
/// over the publish remote so unpushed release edits remain part of the cut.
pub fn previous_release_for_cut(
    entry: &RepoEntry,
    tips: &BookmarkTips,
) -> Option<(BookmarkRef, CommitId)> {
    let scheme = entry.release_scheme();
    newest_release(tips, &scheme, entry.publish_remote())
}

/// The release a record of `name`, a release cut outside knives, follows:
/// [`previous_release_for_cut`] among every release of the series but
/// `name`, whose own refs name the commit recorded.
pub fn previous_release_for_record(
    entry: &RepoEntry,
    tips: &BookmarkTips,
    name: &str,
) -> Option<(BookmarkRef, CommitId)> {
    let scheme = entry.release_scheme();
    newest_release_but(tips, (&scheme, entry.publish_remote()), Some(name))
}

/// The release `name` replaced.
///
/// It is the newest release of the series whose name sorts strictly before
/// `name` in the series' one order ([`ReleaseScheme::cut_key`]). A later
/// release is never one `name` replaced, and `name` with no key, or a fixed
/// branch, replaced none here.
pub fn release_replaced_by(
    entry: &RepoEntry,
    tips: &BookmarkTips,
    name: &str,
) -> Option<(BookmarkRef, CommitId)> {
    let scheme = entry.release_scheme();
    let key = scheme.cut_key(name)?;
    tips.iter()
        .filter(|(reference, _)| is_our_release(reference, &scheme, entry.publish_remote()))
        .filter_map(|(reference, commit)| {
            scheme
                .cut_key(reference.branch().as_str())
                .filter(|other| *other < key)
                .map(|other| (other, reference, commit))
        })
        .max_by_key(|(other, reference, _)| (*other, u8::from(reference.is_local())))
        .map(|(_, reference, commit)| (reference.clone(), commit.clone()))
}

/// Every locally held, non-release, non-trunk branch and its current tip.
pub fn carried_branches(
    repo: &Repo,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> anyhow::Result<Vec<(String, CommitId)>> {
    Ok(carried_from_tips(&repo.bookmark_tips()?, trunk, scheme))
}

/// Every locally held, non-release, non-trunk branch and its current tip.
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

/// Which series a branch belongs to, among an entry's series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeriesFit {
    /// The series this command acts on.
    Selected,
    /// Another series, named as a person reads it ([`RepoEntry::series_label`]).
    Other(String),
    /// No series: why, for the refusal.
    None(String),
}

/// The series a branch tip belongs to: the one whose base it forks from latest.
///
/// A branch of series 9.2 forks from that series' base (say `v9.2+ours`) at
/// or after that base's tip, and from the default trunk only where the two
/// bases split, so its fork point with 9.2's base descends from its fork point
/// with every other base. A tip whose fork points tie or criss-cross fits no
/// series: composing it into one would carry whichever base it really stands
/// on into the other.
///
/// Every series' base takes part. A base that does not resolve, or that has no
/// single fork point with the tip, fits the tip to no series: deciding among
/// the remaining bases would assign the branch by elimination, and a 9.2
/// branch whose base was not fetched would then fit the default series. A
/// base that is an ancestor of another base ties for every branch forked at
/// or below it, so such branches fit no series either.
pub fn series_fit(
    repo: &Repo,
    entry: &RepoEntry,
    tip: &CommitId,
) -> Result<SeriesFit, jj::JjError> {
    let mut points: Vec<(Option<String>, String, CommitId)> = Vec::new();
    let mut unplaced = Vec::new();
    for series in entry.all_series() {
        let base = series.trunk_ref();
        let Ok(base_tip) = repo.resolve_commit(&base) else {
            unplaced.push(format!("{base} does not resolve"));
            continue;
        };
        let Some(point) = repo.common_ancestor(std::slice::from_ref(tip), &base_tip)? else {
            unplaced.push(format!("it has no single fork point with {base}"));
            continue;
        };
        let name = series.selected_series.as_ref().map(ToString::to_string);
        points.push((name, series.series_label(), point));
    }
    if !unplaced.is_empty() {
        return Ok(SeriesFit::None(unplaced.join("; ")));
    }
    let mut latest = None;
    for (index, candidate) in points.iter().enumerate() {
        let mut above_every_other = true;
        for (other_index, other) in points.iter().enumerate() {
            if other_index != index
                && (other.2 == candidate.2 || !repo.is_ancestor(&other.2, &candidate.2)?)
            {
                above_every_other = false;
                break;
            }
        }
        if above_every_other {
            latest = Some(candidate);
            break;
        }
    }
    let Some((name, label, _)) = latest else {
        return Ok(SeriesFit::None(format!(
            "it does not fork from one series' base later than from every other ({})",
            points
                .iter()
                .map(|(_, label, point)| format!("{label} at {}", point.short()))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    let selected = entry.selected_series.as_ref().map(ToString::to_string);
    Ok(if *name == selected {
        SeriesFit::Selected
    } else {
        SeriesFit::Other(label.clone())
    })
}

/// Every declared base of an entry with named series that does not resolve in
/// `repo`, as `<base>@<remote>`. Empty for an entry without named series.
///
/// Membership needs every base: with one missing, its series' branches would
/// fit whichever series is left, so every release verb refuses until it is
/// fetched or the registry names the remote it lives on.
pub fn unresolved_bases(repo: &Repo, entry: &RepoEntry) -> Vec<String> {
    if entry.series.is_empty() {
        return Vec::new();
    }
    entry
        .all_series()
        .iter()
        .map(RepoEntry::trunk_ref)
        .filter(|base| repo.resolve_commit(base).is_err())
        .collect()
}

/// A series' member branches, and a line for each that fits no series.
pub type SeriesMembers = (Vec<(String, CommitId)>, Vec<String>);

/// The branches of `carried` that belong to the selected series.
///
/// Each branch that fits no series yields a line saying why. Another series'
/// branches are left out silently: they are that series' members, and so is
/// every series' base, which members fork from and never are. An entry
/// without named series keeps every branch, so nothing changes for it.
pub fn series_members(
    repo: &Repo,
    entry: &RepoEntry,
    carried: Vec<(String, CommitId)>,
) -> Result<SeriesMembers, jj::JjError> {
    if entry.series.is_empty() {
        return Ok((carried, Vec::new()));
    }
    let bases = entry.bases();
    let mut members = Vec::new();
    let mut unfit = Vec::new();
    for (branch, tip) in carried {
        if bases.contains(&branch.as_str()) {
            continue;
        }
        match series_fit(repo, entry, &tip)? {
            SeriesFit::Selected => members.push((branch, tip)),
            SeriesFit::Other(_) => {}
            SeriesFit::None(why) => {
                unfit.push(format!("{branch} belongs to no release series: {why}"));
            }
        }
    }
    Ok((members, unfit))
}

/// Every position the trunk is known at: the upstream view first, then our
/// fork's, then the local bookmark. Deduplicated, in that order.
///
/// One trunk set for every rule that asks "does the trunk already have this":
/// a member landed upstream, a merge the branch pulled from the trunk, a
/// history measured past the trunk. Measuring past one view alone charged a
/// branch with upstream's own merges whenever the local view of upstream was
/// behind the branch's base, and left the plan and `advance` disagreeing about
/// a parent only the local trunk reached.
pub fn trunk_positions(repo: &Repo, entry: &RepoEntry) -> Result<Vec<CommitId>, jj::JjError> {
    Ok(trunk_positions_in(&repo.bookmark_tips()?, entry))
}

/// [`trunk_positions`], read from `tips`, every bookmark the caller already
/// listed.
pub fn trunk_positions_in(tips: &BookmarkTips, entry: &RepoEntry) -> Vec<CommitId> {
    let trunk = BranchName::new(entry.trunk());
    let views = [
        BookmarkRef::Remote {
            branch: trunk.clone(),
            remote: RemoteName::new(Role::Upstream.to_string()),
        },
        BookmarkRef::Remote {
            branch: trunk.clone(),
            remote: RemoteName::new(Role::Origin.to_string()),
        },
        BookmarkRef::Local(trunk),
    ];
    let mut positions: Vec<CommitId> = Vec::with_capacity(views.len());
    for commit in views.iter().filter_map(|view| tips.get(view)) {
        if !positions.contains(commit) {
            positions.push(commit.clone());
        }
    }
    positions
}

/// The newest release under the configured scheme and publish remote.
///
/// A dated series reads [`ReleaseScheme::cut_key`], the one order of its
/// cuts: a name the series owns but the order cannot read, such as
/// `release/9.2/foo`, has no key and is never the newest.
pub fn newest_release(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> Option<(BookmarkRef, CommitId)> {
    newest_release_but(tips, (scheme, publish_remote), None)
}

/// [`newest_release`] among every release but `except`.
fn newest_release_but(
    tips: &BookmarkTips,
    (scheme, publish_remote): (&ReleaseScheme, &str),
    except: Option<&str>,
) -> Option<(BookmarkRef, CommitId)> {
    let ours = |reference: &BookmarkRef| {
        except != Some(reference.branch().as_str())
            && is_our_release(reference, scheme, publish_remote)
    };
    match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => tips
            .iter()
            .filter(|(reference, _)| ours(reference))
            .filter_map(|(reference, commit)| {
                scheme
                    .cut_key(reference.branch().as_str())
                    .map(|key| (key, reference, commit))
            })
            .max_by_key(|(key, reference, _)| (*key, u8::from(reference.is_local())))
            .map(|(_, reference, commit)| (reference.clone(), commit.clone())),
        ReleaseScheme::Fixed(_) => tips
            .iter()
            .filter(|(reference, _)| ours(reference))
            .max_by_key(|(reference, _)| u8::from(reference.is_local()))
            .map(|(reference, commit)| (reference.clone(), commit.clone())),
    }
}

/// Every commit one of our release refs names, with each ref naming it as a
/// line prints it (`release/2026-08-04@origin`).
///
/// Local refs and refs on the publish remote count; `origin` release-shaped
/// refs in a split-release repository and upstream's own cuts do not, the same
/// trust boundary [`is_our_release`] draws everywhere else.
pub fn release_refs_by_commit(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> BTreeMap<CommitId, Vec<String>> {
    let mut by_commit: BTreeMap<CommitId, Vec<String>> = BTreeMap::new();
    for (reference, commit) in tips {
        if is_our_release(reference, scheme, publish_remote) {
            by_commit
                .entry(commit.clone())
                .or_default()
                .push(reference.to_string());
        }
    }
    by_commit
}

/// Every commit a release of any series of `entry` names, with the names a
/// finding prints.
///
/// Each series' release refs name their commits ([`release_refs_by_commit`]
/// of its scheme and publish remote).
pub fn releases_of_every_series(
    tips: &BookmarkTips,
    entry: &RepoEntry,
) -> BTreeMap<CommitId, Vec<String>> {
    let mut by_commit: BTreeMap<CommitId, Vec<String>> = BTreeMap::new();
    for series in entry.all_series() {
        let scheme = series.release_scheme();
        for (commit, names) in release_refs_by_commit(tips, &scheme, series.publish_remote()) {
            by_commit.entry(commit).or_default().extend(names);
        }
    }
    by_commit
}

/// What every stacked-history check in one run shares: the repository, the
/// trunk positions to measure past, which commits a release names, and what
/// the release already carries.
#[derive(Clone, Copy)]
pub struct StackedHistoryContext<'a> {
    pub repo: &'a Repo,
    /// Every known trunk position ([`trunk_positions`]): a merge counts only
    /// when it joins lines none of them reaches.
    pub trunks: &'a [CommitId],
    /// Which commits a release names, with the names a finding prints: our
    /// release refs ([`release_refs_by_commit`]), or those of every series
    /// ([`releases_of_every_series`]).
    pub releases: &'a BTreeMap<CommitId, Vec<String>>,
    /// Commits whose history the release already carries: the release's
    /// members for an edit (less any it replaces), the other members for a
    /// member of it. Empty where a branch stands alone (a first cut, a pull
    /// request).
    pub carried: &'a [CommitId],
    /// Change ids of the commits the ledger records knives writing under a
    /// release name ([`recorded_release_changes`]): an earlier write no ref
    /// names any more is still a release cut. Read only with `carried`.
    pub written: &'a BTreeSet<crate::ids::ChangeId>,
    /// The member the branch would replace, left out of `carried` for the
    /// merge measure and counted when the commits the branch brings are
    /// listed, so the list holds only what the release would gain.
    pub replaced: Option<&'a CommitId>,
}

/// Change ids of every commit the ledger records knives writing under one of
/// `scheme`'s release names (cut and edit events), as far as this repository
/// holds them; a rewrite keeps its change id.
pub fn recorded_release_changes(
    repo: &Repo,
    entries: &[Entry],
    scheme: &ReleaseScheme,
) -> BTreeSet<crate::ids::ChangeId> {
    recorded_release_names(entries, scheme)
        .into_iter()
        .flat_map(|name| recorded_writes(entries, name))
        .filter_map(|write| repo.change_id_of(write.commit.as_str()).ok())
        .collect()
}

/// Every release name of `scheme` that the ledger records a cut or edit of,
/// read in one pass over `entries`.
pub fn recorded_release_names<'a>(
    entries: &'a [Entry],
    scheme: &ReleaseScheme,
) -> BTreeSet<&'a str> {
    entries
        .iter()
        .filter_map(|event| {
            let subject = event
                .subject
                .as_deref()
                .filter(|subject| scheme.owns(subject))?;
            recorded_write_events(std::slice::from_ref(event), subject)
                .next()
                .map(|_| subject)
        })
        .collect()
}

impl std::fmt::Debug for StackedHistoryContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StackedHistoryContext")
            .field("trunks", &self.trunks)
            .field("releases", &self.releases.len())
            .field("carried", &self.carried)
            .finish_non_exhaustive()
    }
}

/// How a branch's history past the trunk stands for membership.
#[derive(Debug)]
pub enum MemberHistory {
    /// No merge past the trunk.
    Linear,
    /// Merges past the trunk, none of them a release cut, each joining only
    /// lines `carried` already has: everything the branch brings is a single
    /// line of its own. The merges, and how many commits the branch brings.
    CarriedMerges { merges: Vec<CommitId>, own: usize },
    /// Merges that carry more than the branch's own work.
    Stacked(Finding),
}

/// Whether a branch's history past the upstream trunk carries merge commits.
///
/// A member of a flat release is a feature branch: forked from the trunk,
/// linear past it. A merge in that range — a release cut, most often — means the
/// branch carries every parent of that merge as its own content. A cut built
/// from such a member is not flat however many direct parents it has, and a
/// pull request opened from it asks the maintainer to review the whole fork,
/// which is how one branch became an upstream pull request carrying a previous
/// cut's whole composition.
///
/// `None` when the history is linear, or when every merge joins only lines the
/// release already carries ([`member_history`]). The finding names the merges
/// and, for each that a release names ([`StackedHistoryContext::releases`]),
/// which release it is. A merge no release names may be upstream's own,
/// visible only because every local view of the trunk is behind the branch's
/// base; the finding says so, since the local repository cannot exclude it
/// without fetching.
pub fn stacked_history(
    context: StackedHistoryContext<'_>,
    branch: &str,
    tip: &CommitId,
) -> Result<Option<Finding>, jj::JjError> {
    Ok(match member_history(context, branch, tip)? {
        MemberHistory::Stacked(finding) => Some(finding),
        MemberHistory::Linear | MemberHistory::CarriedMerges { .. } => None,
    })
}

/// How `tip`'s history past the trunk stands against what the release carries.
///
/// A merge whose side parents (every parent after the first) the release
/// already carries — a branch built on members of the release, one that
/// merged them to integrate — brings no content but the branch's own line:
/// `git rev-list <tip> ^<release>` lists that line alone. That is
/// [`MemberHistory::CarriedMerges`]. A merge one of our release refs names is a
/// release cut, never a member's own merge, and a merge whose side parent the
/// release lacks carries that side line in: both are
/// [`MemberHistory::Stacked`], and when the release carries anything the
/// finding lists every commit the branch would bring.
pub fn member_history(
    context: StackedHistoryContext<'_>,
    branch: &str,
    tip: &CommitId,
) -> Result<MemberHistory, jj::JjError> {
    let merges = context.repo.merges_between(context.trunks, tip)?;
    if merges.is_empty() {
        return Ok(MemberHistory::Linear);
    }
    let mut named: Vec<String> = merges
        .iter()
        .filter_map(|merge| {
            context
                .releases
                .get(merge)
                .map(|names| format!("{} ({})", names.join(", "), merge.short()))
        })
        .collect();
    if !context.carried.is_empty() {
        for merge in merges
            .iter()
            .filter(|merge| !context.releases.contains_key(*merge))
        {
            if context
                .written
                .contains(&context.repo.change_id_of(merge.as_str())?)
            {
                named.push(format!(
                    "an earlier release write the ledger records ({})",
                    merge.short()
                ));
            }
        }
    }
    let mut bases: Vec<CommitId> = context.trunks.to_vec();
    bases.extend(context.carried.iter().cloned());
    let (uncarried, brought) = if named.is_empty() && !context.carried.is_empty() {
        let uncarried = context.repo.merges_with_side_lines(&bases, tip)?;
        let mut brought_bases = bases.clone();
        brought_bases.extend(context.replaced.cloned());
        let brought = context.repo.commit_ids_between(&brought_bases, tip)?;
        if uncarried.is_empty() {
            return Ok(MemberHistory::CarriedMerges {
                merges,
                own: brought.len(),
            });
        }
        (uncarried, Some(brought))
    } else {
        (merges, None)
    };
    let releases_text = if named.is_empty() {
        "; if the branch forked from a newer upstream than the local trunk views, those \
         are upstream's own merges and `knives sync` fetches them"
            .to_owned()
    } else {
        format!("; releases in that history: {}", named.join("; "))
    };
    let brought_text = brought.map_or_else(String::new, |brought| {
        let listed: Vec<&str> = brought.iter().map(CommitId::short).collect();
        format!(
            "; it brings {} commit(s) the release does not carry: {}",
            brought.len(),
            listed.join(", ")
        )
    });
    Ok(MemberHistory::Stacked(Finding::new(
        FindingKind::StackedHistory,
        Subject::Branch(BranchName::new(branch)),
        format!(
            "branch {branch}'s history past the trunk carries {} merge commit(s), \
             so it carries everything those merges carried{releases_text}{brought_text}",
            uncarried.len()
        ),
    )))
}

/// How a branch tip stands to one released parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The parent is the tip: the member is at its branch tip.
    AtTip,
    /// The tip is a later state of the member: the parent is in its history, or
    /// the parent's change is on the branch under a new commit id.
    Succeeds,
    /// The trunk already reaches the parent. Either the member landed upstream
    /// by merge commit or a legacy cut carried the base as a parent; every
    /// branch forked from the trunk since descends from it, so ancestry says
    /// nothing about which branch was the member. Only the record can.
    Landed,
    Unrelated,
}

/// Whether any trunk position already reaches `commit`.
///
/// One answer for every verb: a commit the trunk reaches is a base, never a
/// member. Taken as a parent — by `include`, or by `advance` moving a member
/// onto a tip the trunk has — it makes the shape a legacy cut has, and
/// [`shared_base`](crate::commands::release::shared_base) then measures every
/// other member from it; the next rebase retires it as redundant. A rebase onto
/// a trunk that has it is the way in.
pub fn trunk_reaches(
    repo: &Repo,
    trunks: &[CommitId],
    commit: &CommitId,
) -> Result<bool, jj::JjError> {
    repo.reached_by(commit, trunks)
}

/// A branch tip's own changes, computed once so every released parent can be
/// tested against it without walking the branch again.
///
/// Two ways for a tip to be a later state of a member. Ancestry: the branch
/// grew and the parent is still in its history. Change identity: the branch
/// was rebased — onto a newer upstream trunk, typically, because a maintainer
/// asked — so the parent commit is hidden and no longer an ancestor, but the
/// change it carried is still on the branch under a new commit id. Ancestry
/// alone called every rebased branch a stranger to its own release: `advance`
/// refused it, `include` would have carried it twice, and agents answered by
/// keeping a second copy of every pull request branch on the old base for the
/// release to carry. One branch is the member and the pull request; this is
/// what lets it move.
#[derive(Debug)]
pub struct MemberSuccession<'a> {
    repo: &'a Repo,
    trunks: &'a [CommitId],
    tip: &'a CommitId,
    /// The tip's change ids past each trunk position: commits the trunk already
    /// has are not the branch's own.
    own_changes: Vec<BTreeSet<crate::ids::ChangeId>>,
}

impl<'a> MemberSuccession<'a> {
    pub fn of(
        repo: &'a Repo,
        trunks: &'a [CommitId],
        tip: &'a CommitId,
    ) -> Result<Self, jj::JjError> {
        let own_changes = trunks
            .iter()
            .map(|trunk| repo.change_ids_between(trunk, tip))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            repo,
            trunks,
            tip,
            own_changes,
        })
    }

    /// How the tip stands to the member whose released parent is `parent`.
    ///
    /// The trunk is asked first: a parent it reaches is [`Relation::Landed`]
    /// whatever the tip's ancestry says, because every branch forked from the
    /// trunk since descends from it. `knives release rebase` is what retires a
    /// landed member; a landed branch that kept growing is found through the
    /// record the cut or edit wrote under its name.
    pub fn relation(&self, parent: &CommitId) -> Result<Relation, jj::JjError> {
        if parent == self.tip {
            return Ok(Relation::AtTip);
        }
        if trunk_reaches(self.repo, self.trunks, parent)? {
            return Ok(Relation::Landed);
        }
        if self.repo.is_ancestor(parent, self.tip)? {
            return Ok(Relation::Succeeds);
        }
        if self.own_changes.is_empty() {
            return Ok(Relation::Unrelated);
        }
        let parent_change = self.repo.change_id_of(parent.as_str())?;
        Ok(
            if self
                .own_changes
                .iter()
                .any(|changes| changes.contains(&parent_change))
            {
                Relation::Succeeds
            } else {
                Relation::Unrelated
            },
        )
    }

    /// Whether the trunk already has the tip itself: the whole branch, merged by
    /// merge commit. A release cut on an older base does not have it, and no
    /// verb should take it as a member — see [`trunk_reaches`].
    pub fn tip_landed(&self) -> Result<bool, jj::JjError> {
        trunk_reaches(self.repo, self.trunks, self.tip)
    }

    /// Whether the tip is a later state of the member whose released parent is
    /// `parent`.
    pub fn succeeds(&self, parent: &CommitId) -> Result<bool, jj::JjError> {
        Ok(self.relation(parent)? == Relation::Succeeds)
    }

    /// Those of `parents` the tip succeeds, in the order given. Several means the
    /// branch's history joins several members' — ambiguous for any verb that
    /// moves one.
    pub fn succeeded_among<'p>(
        &self,
        parents: impl IntoIterator<Item = &'p CommitId>,
    ) -> Result<Vec<CommitId>, jj::JjError> {
        let mut succeeded = Vec::new();
        for parent in parents {
            if self.succeeds(parent)? {
                succeeded.push(parent.clone());
            }
        }
        Ok(succeeded)
    }
}

/// What ties a branch to the release parents it continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberEvidence {
    /// Ancestry or change id: the branch grew, or was rebased by jj.
    Succession,
    /// Only the cut or edit record that named the branch at that parent: it was
    /// rebased outside jj, and nothing in the repository ties them.
    Record,
    /// The record names the branch at a parent the trunk now reaches: the member
    /// landed upstream, and the branch kept going.
    LandedRecord,
}

/// Which of a release's parents a branch continues, and by what evidence.
/// `parents` empty means the branch is not a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberLookup {
    pub parents: Vec<CommitId>,
    pub evidence: MemberEvidence,
}

impl MemberLookup {
    pub const fn is_member(&self) -> bool {
        !self.parents.is_empty()
    }
}

/// The release parents `branch`, at `succession`'s tip, continues among `parents`.
///
/// Succession answers first; the last recorded parent set answers for a branch
/// rebased outside jj or landed upstream, where the repository itself no longer
/// can.
pub fn member_parents(
    succession: &MemberSuccession<'_>,
    parents: &[CommitId],
    recorded: &[RecordedParent],
    branch: &str,
) -> Result<MemberLookup, jj::JjError> {
    let succeeded = succession.succeeded_among(parents)?;
    if !succeeded.is_empty() {
        return Ok(MemberLookup {
            parents: succeeded,
            evidence: MemberEvidence::Succession,
        });
    }
    let named = recorded
        .iter()
        .filter(|parent| parent.branches.iter().any(|name| name == branch))
        .find_map(|parent| {
            parents
                .iter()
                .find(|current| current.as_str() == parent.commit)
        });
    let Some(named) = named else {
        return Ok(MemberLookup {
            parents: Vec::new(),
            evidence: MemberEvidence::Record,
        });
    };
    let evidence = if succession.relation(named)? == Relation::Landed {
        MemberEvidence::LandedRecord
    } else {
        MemberEvidence::Record
    };
    Ok(MemberLookup {
        parents: vec![named.clone()],
        evidence,
    })
}

/// Every maintained branch's succession, computed once so each released parent
/// can be asked where its branch has gone without walking the branches again.
///
/// "Carries no bookmark" is a poor report of a stale parent. The useful answer
/// is which branch that commit belonged to and where it moved, and after a
/// `jj rebase` only the change id still says so. Release names and the trunk
/// are not candidates; `branches` is the caller's list of maintained branches.
#[derive(Debug)]
pub struct BranchSuccessions<'a> {
    branches: Vec<(&'a str, &'a CommitId, MemberSuccession<'a>)>,
}

impl<'a> BranchSuccessions<'a> {
    pub fn of(
        repo: &'a Repo,
        trunks: &'a [CommitId],
        branches: &'a [(String, CommitId)],
    ) -> Result<Self, jj::JjError> {
        let branches = branches
            .iter()
            .map(|(branch, tip)| {
                MemberSuccession::of(repo, trunks, tip)
                    .map(|succession| (branch.as_str(), tip, succession))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { branches })
    }

    /// The branches whose tip is a later state of the member released as
    /// `parent`.
    pub fn successors_of(&self, parent: &CommitId) -> Result<Vec<(String, CommitId)>, jj::JjError> {
        let mut found = Vec::new();
        for (branch, tip, succession) in &self.branches {
            if succession.succeeds(parent)? {
                found.push(((*branch).to_owned(), (*tip).clone()));
            }
        }
        Ok(found)
    }
}

/// Detect release names that refer to different trees in trusted refs.
pub fn double_cut_findings(
    repo_path: &Path,
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> anyhow::Result<(Vec<Finding>, Vec<String>)> {
    let disagreements =
        crate::detect::double_cut::same_name_disagreements(tips, scheme, publish_remote);
    if disagreements.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut findings = Vec::new();
    let mut notes = Vec::new();
    for (name, references) in disagreements {
        let mut commits: BTreeSet<CommitId> =
            references.into_iter().map(|(_, commit)| commit).collect();
        let Some(first) = commits.pop_first() else {
            anyhow::bail!("double-cut disagreement for {name} named no commits");
        };
        let mut changed = BTreeSet::new();
        let mut different = None;
        for other in commits {
            let files = jj::changed_files_between(repo_path, first.as_str(), other.as_str())?;
            if !files.is_empty() && different.is_none() {
                different = Some(other);
            }
            changed.extend(files);
        }
        if changed.is_empty() {
            notes.push(format!(
                "{name} names two commits with identical trees (a rebuilt cut)"
            ));
        } else if let Some(different) = different {
            findings.push(Finding::new(
                FindingKind::DoubleCut,
                Subject::Branch(name.clone()),
                format!(
                    "{name} names both {} and {}, and their trees differ ({} files)",
                    first.short(),
                    different.short(),
                    changed.len()
                ),
            ));
        } else {
            anyhow::bail!("double-cut disagreement for {name} had no tree comparison");
        }
    }
    Ok((findings, notes))
}

/// The composition a recorded write's ledger event recorded: the previous
/// cut's ([`last_recorded_cut`]), or the newest write of the series' newest
/// recorded release ([`newest_release_write`]).
#[derive(Debug, PartialEq, Eq)]
pub struct RecordedCut {
    /// Whether the event records a cut or an edit.
    pub kind: RecordedWriteKind,
    pub name: String,
    /// The commit the write created, stored as the first evidence item.
    pub commit: CommitId,
    pub members: Vec<CommitId>,
}

/// The parent set a cut or edit records: each parent with every carried branch
/// at its commit when the record was written.
///
/// Every name is kept, not the first. A parent's branch may share its tip with
/// an anchor bookmark another agent set (`keep/…`, `anchor/…`), and a record
/// that named only the alphabetically first of them would fail to recognise
/// the member once its own bookmark moved on - the exact case this record
/// exists for.
pub fn parents_with_branches(
    tips: &BookmarkTips,
    trunk: &str,
    scheme: &ReleaseScheme,
    parents: &[CommitId],
) -> Vec<RecordedParent> {
    let carried = carried_from_tips(tips, trunk, scheme);
    parents
        .iter()
        .map(|commit| RecordedParent {
            commit: commit.as_str().to_owned(),
            branches: carried
                .iter()
                .filter(|(_, tip)| tip == commit)
                .map(|(branch, _)| branch.clone())
                .collect(),
        })
        .collect()
}

/// `feat/alpha@0123456789ab, fix/beta@…`: the parent list a cut or edit event's
/// text shows a reader. Prose only; the record is [`Entry::parents`].
pub fn members_event_text(members: &[(String, CommitId)]) -> String {
    members
        .iter()
        .map(|(source, commit)| format!("{source}@{}", commit.short()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The newest parent set recorded for `release`: the last cut, include, drop,
/// advance or rebase event that wrote one, a withdrawn write's aside
/// ([`Withdrawn`]). Empty when nothing was recorded.
pub fn last_recorded_parents<'a>(entries: &'a [Entry], release: &str) -> &'a [RecordedParent] {
    let withdrawn = Withdrawn::read(entries);
    entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(release))
        .filter(|entry| {
            entry
                .evidence
                .first()
                .is_none_or(|write| !withdrawn.withdraws(release, &CommitId::new(write.as_str())))
        })
        .map(|entry| entry.parents.as_slice())
        .find(|parents| !parents.is_empty())
        .unwrap_or_default()
}

/// The member a successor of `branch` replaces, or `None`.
///
/// The one member `lookup` found, or, among several, the one the last event
/// recorded under this branch's own name (`recorded`). `None` when there is
/// none, or the members are ambiguous.
pub fn replaced_member<'a>(
    recorded: &[RecordedParent],
    lookup: &'a MemberLookup,
    branch: &str,
) -> Option<&'a CommitId> {
    match lookup.parents.as_slice() {
        [] => None,
        [member] => Some(member),
        many => {
            let mut own = many
                .iter()
                .filter(|member| recorded_under(recorded, member, branch));
            match (own.next(), own.next()) {
                (Some(member), None) => Some(member),
                _ => None,
            }
        }
    }
}

/// Whether the last event recorded `member` under the bookmark `branch`.
fn recorded_under(recorded: &[RecordedParent], member: &CommitId, branch: &str) -> bool {
    recorded.iter().any(|parent| {
        parent.commit == member.as_str() && parent.branches.iter().any(|name| name == branch)
    })
}

/// Whether a merge branch builds on the members it descends from, and so
/// joins beside them through `include`, rather than continuing one of them.
///
/// The branch's history carries merges the release already carries, and
/// `lookup` found its members by succession. A branch one of whose members
/// the last event recorded under its own name continues that member, however
/// many others it merged: `advance` replaces that one and measures it against
/// the rest. Otherwise, several members: it merges them. One: it merges that
/// member when, measured against the other members, its history carries that
/// member's line ([`MemberHistory::Stacked`]).
pub fn integrates(
    context: StackedHistoryContext<'_>,
    recorded: &[RecordedParent],
    lookup: &MemberLookup,
    (branch, tip): (&str, &CommitId),
) -> Result<bool, jj::JjError> {
    if lookup.evidence != MemberEvidence::Succession
        || lookup
            .parents
            .iter()
            .any(|member| recorded_under(recorded, member, branch))
        || !matches!(
            member_history(context, branch, tip)?,
            MemberHistory::CarriedMerges { .. }
        )
    {
        return Ok(false);
    }
    match lookup.parents.as_slice() {
        [] => Ok(false),
        [member] => Ok(matches!(
            carries_member_line(context, member, (branch, tip))?,
            MemberHistory::Stacked(_)
        )),
        _ => Ok(true),
    }
}

/// `tip`'s history measured against the members other than `member`, as an
/// `advance` that replaces `member` measures it.
pub fn carries_member_line(
    context: StackedHistoryContext<'_>,
    member: &CommitId,
    (branch, tip): (&str, &CommitId),
) -> Result<MemberHistory, jj::JjError> {
    let others: Vec<CommitId> = context
        .carried
        .iter()
        .filter(|parent| *parent != member)
        .cloned()
        .collect();
    member_history(
        StackedHistoryContext {
            carried: &others,
            replaced: Some(member),
            ..context
        },
        branch,
        tip,
    )
}

/// The newest structural cut event, optionally scoped to a release name: the
/// newest cut event ([`cut_record`]) whose write no event withdraws
/// ([`Withdrawn`]).
pub fn last_recorded_cut(entries: &[Entry], subject: Option<&str>) -> Option<RecordedCut> {
    let withdrawn = Withdrawn::read(entries);
    entries
        .iter()
        .rev()
        .filter(|entry| subject.is_none_or(|subject| entry.subject.as_deref() == Some(subject)))
        .filter_map(cut_record)
        .find(|cut| !withdrawn.withdraws(&cut.name, &cut.commit))
}

/// The cut `entry` records, read from it alone: an event whose text begins
/// `cut <subject> as `, its evidence the commit and then its parents. The
/// members are the parents less each one that its parent set labels as a
/// carrier ([`carrier_label`]): the published commit of a fixed branch that a
/// cut records may have the release it followed as a parent, and that is an
/// earlier write, never a member. `None` for any other entry, and for a cut
/// event with no member.
fn cut_record(entry: &Entry) -> Option<RecordedCut> {
    let subject = entry.subject.as_deref()?;
    if entry.kind != Kind::Event || !entry.text.starts_with(&format!("cut {subject} as ")) {
        return None;
    }
    let commit = entry.evidence.first()?;
    let carrier = carrier_label(subject);
    let members = event_members(entry, |parent| parent.branches.contains(&carrier));
    if members.is_empty() {
        return None;
    }
    Some(RecordedCut {
        kind: RecordedWriteKind::Cut,
        name: subject.to_owned(),
        commit: CommitId::new(commit.as_str()),
        members,
    })
}

/// What the text of an event that withdraws a recorded write begins with,
/// after `withdrew ` and the release ([`withdrawal_text`]).
const WITHDREW: &str = "'s recorded write ";

/// The text of the event that withdraws a recorded write of `release`.
///
/// `write`, of `kind`, is one that `remote`, the publish remote, never
/// held: `withdrew <release>'s recorded write (a cut) <commit>: …`, naming
/// `published`, the commit that `cut --corrects` records in its place. The
/// event's evidence is the write, then `published` ([`withdrawn_write`]).
pub fn withdrawal_text(
    release: &str,
    (kind, write): (RecordedWriteKind, &CommitId),
    (remote, published): (&str, &CommitId),
) -> String {
    let kind = match kind {
        RecordedWriteKind::Cut => "a cut",
        RecordedWriteKind::Edit => "an edit",
    };
    format!(
        "withdrew {release}{WITHDREW}({kind}) {}: {release}@{remote} never held it, and the cut \
         of {} records the published commit in its place",
        write.short(),
        published.short()
    )
}

/// The recorded write of `release` that `entry` withdraws.
///
/// It is the entry's first evidence, where it is an event of `release`
/// whose text begins `withdrew <release>'s recorded write `
/// ([`withdrawal_text`]). `None` for any other entry.
pub fn withdrawn_write(entry: &Entry, release: &str) -> Option<CommitId> {
    if entry.kind != Kind::Event || entry.subject.as_deref() != Some(release) {
        return None;
    }
    entry
        .text
        .strip_prefix("withdrew ")?
        .strip_prefix(release)?
        .strip_prefix(WITHDREW)?;
    entry
        .evidence
        .first()
        .map(|write| CommitId::new(write.as_str()))
}

/// Every recorded write that an event of `entries` withdraws, each with its
/// release ([`withdrawn_write`]).
///
/// A write of a fixed branch that the publish remote never held is withdrawn
/// by the cut that records the branch's published commit in its place
/// (`cut --corrects`). Every reader of the release's writes then reads it as
/// never written: it is no previous cut, no newest write, no earlier write and
/// no reference of a fix by hand ([`recorded_write_events`],
/// [`last_recorded_cut`], [`last_series_cut`], [`last_recorded_parents`]).
#[derive(Debug, Default)]
pub struct Withdrawn<'a>(Vec<(&'a str, CommitId)>);

impl<'a> Withdrawn<'a> {
    pub fn read(entries: &'a [Entry]) -> Self {
        Self(
            entries
                .iter()
                .filter_map(|entry| {
                    let release = entry.subject.as_deref()?;
                    withdrawn_write(entry, release).map(|write| (release, write))
                })
                .collect(),
        )
    }

    /// Whether `commit`, a recorded write of `release`, is withdrawn. A
    /// ledger may give either commit short, so one names the other where it
    /// begins with it and gives at least as much of it as a short commit
    /// does, 12 characters: a shorter id, as a peer's ledger can carry, would
    /// withdraw every write whose commit begins with it.
    pub fn withdraws(&self, release: &str, commit: &CommitId) -> bool {
        let commit = commit.as_str();
        self.0.iter().any(|(from, withdrawn)| {
            let (shorter, longer) = if commit.len() <= withdrawn.as_str().len() {
                (commit, withdrawn.as_str())
            } else {
                (withdrawn.as_str(), commit)
            };
            *from == release
                && !shorter.is_empty()
                && shorter.len() >= longer.len().min(12)
                && longer.starts_with(shorter)
        })
    }
}

/// The members a cut or edit event records: its evidence after the commit,
/// less each commit that its parent set records under a parent `skip`
/// takes, such as a carrier's.
fn event_members(event: &Entry, skip: impl Fn(&RecordedParent) -> bool) -> Vec<CommitId> {
    event
        .evidence
        .iter()
        .skip(1)
        .filter(|commit| {
            !event
                .parents
                .iter()
                .any(|parent| parent.commit == **commit && skip(parent))
        })
        .map(|commit| CommitId::new(commit.as_str()))
        .collect()
}

/// Which of knives' own writes a ledger event records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedWriteKind {
    Cut,
    Edit,
}

impl std::fmt::Display for RecordedWriteKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Cut => "cut",
            Self::Edit => "edit",
        })
    }
}

/// The commit a cut or edit event says knives wrote under a release name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedWrite {
    pub kind: RecordedWriteKind,
    pub commit: CommitId,
}

/// Every commit knives itself put under `release`, newest first.
///
/// That is what each cut event or edit event (`include`, `drop`, `advance`,
/// `rebase`, and `adopt`, which takes a release finished by hand in as
/// knives' own with an edit event) records first in its evidence.
///
/// An edit writes a new commit under the same name, so a release that equals
/// its last edit is knives' own work, not drift, and a publish remote at an
/// earlier write is knives' own work an edit has not been pushed over. A
/// cut's composition gate reads the newest write of the series' newest
/// release ([`newest_release_write`]) beside the previous cut
/// ([`last_recorded_cut`]), so a member an edit dropped is still restated at
/// the next cut, and a member an edit added is held ([`CompositionBaseline`]).
pub fn recorded_writes<'a>(
    entries: &'a [Entry],
    release: &'a str,
) -> impl Iterator<Item = RecordedWrite> + 'a {
    recorded_write_events(entries, release).map(|(_, write)| write)
}

/// [`recorded_writes`], each beside the whole ledger event that records it.
///
/// A write that replaces the newest one copies its event's `parents`: each
/// member with the branch names it carried when it was written, which is what
/// `include` and `advance` match a member by once its bookmark has moved on.
pub fn recorded_write_events<'a>(
    entries: &'a [Entry],
    release: &'a str,
) -> impl Iterator<Item = (&'a Entry, RecordedWrite)> + 'a {
    let withdrawn = Withdrawn::read(entries);
    entries
        .iter()
        .rev()
        .filter_map(move |entry| {
            if entry.kind != Kind::Event || entry.subject.as_deref() != Some(release) {
                return None;
            }
            if let Some(cut) = cut_record(entry) {
                return Some((
                    entry,
                    RecordedWrite {
                        kind: RecordedWriteKind::Cut,
                        commit: cut.commit,
                    },
                ));
            }
            let rest = entry.text.strip_prefix("edited ")?.strip_prefix(release)?;
            if !(rest.starts_with(':') || rest.starts_with(' ')) {
                return None;
            }
            Some((
                entry,
                RecordedWrite {
                    kind: RecordedWriteKind::Edit,
                    commit: CommitId::new(entry.evidence.first()?.as_str()),
                },
            ))
        })
        .filter(move |(_, write)| !withdrawn.withdraws(release, &write.commit))
}

/// The newest recorded write of the newest release that `entries` record in
/// the series `scheme`, `except` aside: what a cut is held against beside
/// the series' previous cut ([`CompositionBaseline`]).
///
/// It is the first of the series' writes ([`series_write_events`]): the
/// newest release's cut, its newest edit or an `adopt`, whichever the
/// ledger records last. Under a fixed scheme the release is the fixed
/// branch, the one name that the scheme owns. The members are that
/// event's ([`event_members`]), less each parent that its parent set
/// labels as a carrier ([`carrier_label`]). So a member that `include`
/// added after the cut is held.
pub fn newest_release_write(
    entries: &[Entry],
    scheme: &ReleaseScheme,
    except: Option<&str>,
) -> Option<RecordedCut> {
    let (name, event, write) = series_write_events(entries, scheme, except).next()?;
    Some(RecordedCut {
        kind: write.kind,
        name: name.to_owned(),
        commit: write.commit,
        members: recorded_write_members(event, name),
    })
}

/// The members that `event`, a cut or edit event of `release`, records:
/// its evidence after the write ([`event_members`]), less each parent that
/// its parent set labels as a carrier ([`carrier_label`]).
pub fn recorded_write_members(event: &Entry, release: &str) -> Vec<CommitId> {
    let carrier = carrier_label(release);
    event_members(event, |parent| parent.branches.contains(&carrier))
}

/// A recorded write of a series ([`series_writes`]): the release it wrote
/// and its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesWrite<'a> {
    /// The release, a ledger entry's subject.
    pub name: &'a str,
    /// The write's commit.
    pub commit: CommitId,
}

/// Every write that `entries` record in the series `scheme`, `except`
/// aside, newest first, each with its release: what a fix by hand is held
/// against, file by file ([`crate::commands::release::adopted_fixes`]).
///
/// They are [`series_write_events`], so the first is the newest recorded
/// write ([`newest_release_write`]), and each later one is older.
pub fn series_writes<'a>(
    entries: &'a [Entry],
    scheme: &ReleaseScheme,
    except: Option<&str>,
) -> Vec<SeriesWrite<'a>> {
    series_write_events(entries, scheme, except)
        .map(|(name, _, write)| SeriesWrite {
            name,
            commit: write.commit,
        })
        .collect()
}

/// The one order of a series' writes: every write that `entries` record in
/// the series `scheme`, `except` aside, newest first, each with its release
/// and its event.
///
/// The releases are the names that the ledger records a cut or edit of
/// ([`recorded_release_names`]), newest first in the series' one order
/// ([`ReleaseScheme::cut_key`]); under a fixed scheme there is one, the
/// fixed branch. Each release's writes come newest first
/// ([`recorded_write_events`]).
fn series_write_events<'a>(
    entries: &'a [Entry],
    scheme: &ReleaseScheme,
    except: Option<&str>,
) -> impl Iterator<Item = (&'a str, &'a Entry, RecordedWrite)> {
    let mut names: Vec<_> = recorded_release_names(entries, scheme)
        .into_iter()
        .filter(|name| Some(*name) != except)
        .filter_map(|name| match scheme {
            ReleaseScheme::Fixed(_) => Some((None, name)),
            ReleaseScheme::Dated | ReleaseScheme::Series(_) => {
                scheme.cut_key(name).map(|key| (Some(key), name))
            }
        })
        .collect();
    names.sort_unstable_by(|first, second| second.cmp(first));
    names.into_iter().flat_map(move |(_, name)| {
        recorded_write_events(entries, name).map(move |(event, write)| (name, event, write))
    })
}

/// What a cut or edit event's text puts before the mark of a write over a
/// release in hand knives could not check: `; unchecked: <reason>` closes the
/// text ([`unchecked_mark`]).
pub const UNCHECKED_DELIMITER: &str = "; unchecked: ";

/// The mark of a write whose matched recorded write this repository does
/// not hold.
pub const UNCHECKED_WRITE_ABSENT: &str = "W absent";

/// The mark of a write over a release whose members match no recorded write.
pub const UNCHECKED_NO_MATCH: &str = "no match";

/// The mark of a write over a release whose name the ledger records no write
/// of.
pub const UNCHECKED_NO_RECORD: &str = "no record";

/// Why knives could not check the release in hand a cut or edit event wrote
/// over.
///
/// It is the reason after the [`UNCHECKED_DELIMITER`] the text ends with,
/// after every part a reader parses, and only one of the three reasons a
/// write marks ([`UNCHECKED_WRITE_ABSENT`], [`UNCHECKED_NO_MATCH`],
/// [`UNCHECKED_NO_RECORD`]): a caller's reason inside the text that happens
/// to hold the delimiter is no mark. `None` for every other event.
pub fn unchecked_mark(entry: &Entry) -> Option<&'static str> {
    [
        UNCHECKED_WRITE_ABSENT,
        UNCHECKED_NO_MATCH,
        UNCHECKED_NO_RECORD,
    ]
    .into_iter()
    .find(|reason| {
        entry
            .text
            .strip_suffix(reason)
            .is_some_and(|rest| rest.ends_with(UNCHECKED_DELIMITER))
    })
}

/// The change id a cut or edit event of `release` records for its commit.
///
/// Both texts carry it: `cut <release> as <commit> (change <id>) …` and
/// `edited <release> as <commit> (change <id>) …`, as the id's 12-character
/// prefix ([`crate::ids::short_id`]). `None` for an event an older knives
/// wrote without one.
fn recorded_change<'a>(entry: &'a Entry, release: &str) -> Option<&'a str> {
    let rest = entry
        .text
        .strip_prefix("cut ")
        .or_else(|| entry.text.strip_prefix("edited "))?;
    let (_, rest) = rest
        .strip_prefix(release)?
        .strip_prefix(" as ")?
        .split_once(' ')?;
    let (change, _) = rest.strip_prefix("(change ")?.split_once(')')?;
    (!change.is_empty()).then_some(change)
}

/// What the part of an `adopt` event that names the files resolved by hand
/// begins with.
pub const ADOPTED_RESOLVED: &str = "resolved by hand: ";

/// What the part of an `adopt` event that names the files changed beyond
/// the baseline's conflicts begins with.
pub const ADOPTED_BEYOND: &str = "changed beyond the baseline's conflicts: ";

/// The write that an `adopt` event of `release` replaced, when the adopt
/// recorded a fix by hand.
///
/// The write is its commit's 12-character prefix ([`crate::ids::short_id`]).
/// A fix by hand is a file resolved by hand or changed beyond the adopt's
/// baseline's conflicts. An adopt is an edit event whose delta begins
/// `adopted: replaces the recorded <kind> <commit>; measured against
/// <baseline>`. The parts that name files follow it, [`ADOPTED_RESOLVED`]
/// first, then [`ADOPTED_BEYOND`]. Knives wrote every word before them, so
/// the reason that a caller gives, which follows them, never reads as one.
/// `None` for every other event, and for an adopt that changed no file.
pub fn adopted_fix_replaces<'a>(entry: &'a Entry, release: &str) -> Option<&'a str> {
    adopted_fix_parts(entry, release).map(|(commit, _)| commit)
}

/// The files that an `adopt` event names as its fix by hand
/// ([`AdoptedFixText::files`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptedFixFiles {
    /// The files resolved by hand, then those changed beyond the baseline's
    /// conflicts, each path once. Read from the event's text, they are every
    /// path the text may name ([`text_paths`]).
    pub files: Vec<String>,
    /// Whether they were read from the event's text, as for an event without
    /// a `fix_files` list ([`Entry::fix_files`]), which an older knives
    /// writes. The text joins a list's files with `, ` and ends the list at
    /// `; ` or ` (why: `, so it cannot tell a path that holds one of those
    /// from two paths ([`adopted_fix_text`]). A reader holds only those of
    /// [`Self::files`] that the adopt's own tree holds, beside the files
    /// that its change changes.
    pub from_text: bool,
}

/// What an `adopt` event's text joins the files of a list with, or cuts
/// the list at, and so what a path that holds one reads back cut at.
const TEXT_SEPARATORS: [&str; 3] = [", ", "; ", " (why: "];

/// The most pieces that a path read from a list of an `adopt` event's text
/// joins ([`text_paths`]).
///
/// A path that holds up to 15 separators reads back whole; a path that
/// holds more is not read, as a path that a person names holds a few
/// separators at most, and the adopt's own change names its files. The cap
/// bounds the reading of a list of `n` pieces, which any peer's ledger may
/// carry, to at most `16 * n` paths of at most 16 pieces each: every run of
/// its pieces would be `n * (n + 1) / 2` paths, each up to the whole list
/// long.
pub const TEXT_PATH_PIECES: usize = 16;

/// Where each piece of `list`, one list of files in an `adopt` event's
/// text, begins and ends, in order: the text cuts it at each separator
/// ([`TEXT_SEPARATORS`]), the first one after each piece. Each separator
/// is searched for again only once the pieces have passed its last match,
/// so the list is read once for each separator.
fn text_pieces(list: &str) -> impl Iterator<Item = (usize, usize)> {
    let find = move |separator: &str, from: usize| {
        list.get(from..)
            .and_then(|rest| rest.find(separator))
            .map(|offset| from + offset)
    };
    let mut next = TEXT_SEPARATORS.map(|separator| find(separator, 0));
    let mut start = Some(0);
    std::iter::from_fn(move || {
        let from = start?;
        let Some((end, length)) = TEXT_SEPARATORS
            .iter()
            .zip(next)
            .filter_map(|(separator, at)| Some((at?, separator.len())))
            .min_by_key(|(at, _)| *at)
        else {
            start = None;
            return Some((from, list.len()));
        };
        let after = end + length;
        start = Some(after);
        for (found, separator) in next.iter_mut().zip(TEXT_SEPARATORS) {
            if found.is_some_and(|at| at < after) {
                *found = find(separator, after);
            }
        }
        Some((from, end))
    })
}

/// Every path that `lists`, the lists of files in an `adopt` event's text,
/// may name: each run of at most [`TEXT_PATH_PIECES`] pieces of a list
/// ([`text_pieces`]), joined back with the separators the text holds
/// between them. So `x, y.txt` gives `x`, `x, y.txt` and `y.txt`: two paths,
/// or one path that holds `, `. Each path once, in the order that the text
/// names them, as a slice of the text.
fn text_paths<'a>(lists: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let (mut paths, mut named) = (Vec::new(), HashSet::new());
    for list in lists {
        let pieces: Vec<(usize, usize)> = text_pieces(list).collect();
        for (piece, (start, _)) in pieces.iter().enumerate() {
            for (_, end) in pieces.iter().skip(piece).take(TEXT_PATH_PIECES) {
                if let Some(path) = list.get(*start..*end)
                    && !path.is_empty()
                    && named.insert(path)
                {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

/// What an `adopt` event of `release` names as the files of its fix by
/// hand, read once ([`AdoptedFixText::files`]).
///
/// The files are those it resolved by hand ([`ADOPTED_RESOLVED`]), then
/// those it changed beyond the baseline's conflicts ([`ADOPTED_BEYOND`]), as
/// `adopt` measured them where it ran. It names none for every event that
/// [`adopted_fix_replaces`] reads as no fix.
///
/// The event's `fix_files` list holds each path whole
/// ([`Entry::fix_files`]), and a reader takes it first. An event without
/// the list, as an older knives writes one, names the files in its text
/// alone ([`AdoptedFixFiles::from_text`]): each list as its files joined by
/// `, `, then the event's other parts after `; `, in the order that `adopt`
/// writes them ([`RESOLVED_LIST_ENDS`]), and its parents last, after
/// `; parents: `. Every `adopt` event ends with its parents, which name
/// refs and commits, so the last `; parents: ` is the one that `adopt`
/// wrote. The resolved list ends where the other list begins, at the first
/// `; changed beyond the baseline's conflicts: `, or, where no other list
/// follows, at the first `; why: `, `; carriers: ` or
/// `; added lines absent: `. The other list ends where its reason begins,
/// at its last ` (why: `, or else at the next `; `, so it reads at least
/// whole. A path that holds a separator does not read back whole from a
/// list, so each list gives every path of at most [`TEXT_PATH_PIECES`]
/// pieces that it may name ([`text_paths`]).
pub fn adopted_fix_text<'a>(entry: &'a Entry, release: &str) -> AdoptedFixText<'a> {
    let Some((_, parts)) = adopted_fix_parts(entry, release) else {
        return AdoptedFixText(FixNames::Nothing);
    };
    if !entry.fix_files.is_empty() {
        return AdoptedFixText(FixNames::Listed(&entry.fix_files));
    }
    let parts = parts
        .rsplit_once("; parents: ")
        .map_or(parts, |(parts, _)| parts);
    let (resolved, beyond) = text_lists(parts);
    AdoptedFixText(FixNames::Text { resolved, beyond })
}

/// What an `adopt` event names as the files of its fix by hand, read once
/// ([`adopted_fix_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdoptedFixText<'a>(FixNames<'a>);

/// Where an `adopt` event names the files of its fix ([`AdoptedFixText`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixNames<'a> {
    /// Nowhere: the event records no fix by hand.
    Nothing,
    /// Its `fix_files` list ([`Entry::fix_files`]).
    Listed(&'a [FixFile]),
    /// Its text alone, as an older knives writes it: the files resolved by
    /// hand, then those changed beyond the baseline's conflicts
    /// ([`text_lists`]).
    Text {
        resolved: &'a str,
        beyond: Option<&'a str>,
    },
}

impl AdoptedFixText<'_> {
    /// The files of the fix, each path once: the `fix_files` list, or every
    /// path that the text's lists may name ([`text_paths`]), at most
    /// [`TEXT_PATH_PIECES`] for each piece of a list.
    pub fn files(&self) -> AdoptedFixFiles {
        match self.0 {
            FixNames::Nothing => AdoptedFixFiles::default(),
            FixNames::Listed(listed) => {
                let mut named = HashSet::new();
                AdoptedFixFiles {
                    files: listed
                        .iter()
                        .filter(|file| named.insert(file.path.as_str()))
                        .map(|file| file.path.clone())
                        .collect(),
                    from_text: false,
                }
            }
            FixNames::Text { resolved, beyond } => AdoptedFixFiles {
                files: text_paths(std::iter::once(resolved).chain(beyond))
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                from_text: true,
            },
        }
    }
}

/// The parts that `adopt` writes after an event's list of files resolved by
/// hand, each after `; `, in their order: the files changed beyond the
/// baseline's conflicts ([`ADOPTED_BEYOND`]), or else the reason, then the
/// carriers, then the added lines absent from the resolution.
const RESOLVED_LIST_ENDS: [&str; 4] = [
    "; changed beyond the baseline's conflicts: ",
    "; why: ",
    "; carriers: ",
    "; added lines absent: ",
];

/// The lists of files that `parts`, an `adopt` event's parts after its
/// baseline ([`adopted_fix_parts`]), up to its parents, name in its text:
/// the files resolved by hand, empty where it resolved none, then the files
/// changed beyond the baseline's conflicts, where it changed any. The
/// resolved list ends where the other list begins, at the first
/// `; changed beyond the baseline's conflicts: `, or, where no other list
/// follows, at the first of the other parts that `adopt` writes after it
/// ([`RESOLVED_LIST_ENDS`]). The other list ends where its reason begins,
/// at its last ` (why: `, or else at the next `; ` ([`adopted_fix_text`]).
fn text_lists(parts: &str) -> (&str, Option<&str>) {
    let [beyond_part, after @ ..] = RESOLVED_LIST_ENDS;
    let (resolved, beyond) = parts.strip_prefix(ADOPTED_RESOLVED).map_or_else(
        || ("", parts.strip_prefix(ADOPTED_BEYOND)),
        |resolved| {
            resolved.split_once(beyond_part).map_or_else(
                || {
                    let end = after.iter().filter_map(|part| resolved.find(part)).min();
                    (
                        end.and_then(|end| resolved.get(..end)).unwrap_or(resolved),
                        None,
                    )
                },
                |(list, beyond)| (list, Some(beyond)),
            )
        },
    );
    let beyond = beyond.map(|beyond| {
        beyond
            .rsplit_once(" (why: ")
            .or_else(|| beyond.split_once("; "))
            .map_or(beyond, |(list, _)| list)
    });
    (resolved, beyond)
}

/// The replaced write's commit prefix and the parts after the baseline of
/// an `adopt` event of `release` that recorded a fix by hand
/// ([`adopted_fix_replaces`]).
fn adopted_fix_parts<'a>(entry: &'a Entry, release: &str) -> Option<(&'a str, &'a str)> {
    let rest = entry
        .text
        .strip_prefix("edited ")?
        .strip_prefix(release)?
        .strip_prefix(" as ")?;
    let (_, delta) = rest.split_once(" parent(s): ")?;
    let replaced = delta.strip_prefix("adopted: replaces the recorded ")?;
    let replaced = replaced
        .strip_prefix("cut ")
        .or_else(|| replaced.strip_prefix("edit "))?;
    let (commit, measured) = replaced.split_once("; measured against ")?;
    let (_, parts) = measured.split_once("; ")?;
    (parts.starts_with(ADOPTED_RESOLVED) || parts.starts_with(ADOPTED_BEYOND))
        .then_some((commit, parts))
}

/// Every earlier write of one release name.
///
/// That is each commit the ledger records as a cut or an edit of the name
/// ([`recorded_writes`]), the publish remote's commit of the name as the
/// caller reads that ref, every visible rewrite of either
/// ([`Repo::rewrites_of`]), every visible commit whose change id a
/// recorded cut or edit event names, and every parent a recorded event lists
/// with the carrier label ([`carrier_label`]).
///
/// A `jj rebase` or `jj describe` that moves a recorded write leaves the
/// recorded id hidden and the commit that matters under a new id with the
/// same change id. A write pushed as a fast-forward copy keeps its change id
/// too, so a repository that holds none of the recorded commits, a fresh
/// clone of the fork, still reads the copies as earlier writes. A recorded
/// write is a release merge and a member is a feature branch, so the two
/// never share a change id, and neither test can take a member for a write.
/// A recorded write this repository does not hold is no commit here; its id
/// still marks it a write where a recorded parent set names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EarlierWrites {
    /// The writes this repository holds, each once: recorded writes newest
    /// first, then the published commit, each followed by its rewrites, then
    /// the copies found by a recorded change id.
    held: Vec<CommitId>,
    /// Every recorded write's commit, held here or not.
    recorded: Vec<CommitId>,
    /// Every change id a recorded event names, as the 12-character prefix.
    changes: Vec<String>,
    /// Every parent a recorded event lists with the carrier label
    /// ([`carrier_label`]): only a recorded write's own record of its
    /// fast-forward parent writes that label, so the commit is a carrier
    /// though no event records it as a write (a resolution pushed on top of
    /// a write, then a parent of the release made again on it).
    labelled: Vec<CommitId>,
}

impl EarlierWrites {
    /// The earlier writes of `release`.
    pub fn read(
        repo: &Repo,
        release: &str,
        entries: &[Entry],
        published: Option<&CommitId>,
    ) -> Result<Self, jj::JjError> {
        let mut writes = Self::default();
        let label = carrier_label(release);
        for (entry, write) in recorded_write_events(entries, release) {
            if let Some(change) = recorded_change(entry, release)
                && !writes.changes.iter().any(|known| known == change)
            {
                writes.changes.push(change.to_owned());
            }
            if !writes.recorded.contains(&write.commit) {
                writes.recorded.push(write.commit);
            }
            for parent in &entry.parents {
                let commit = CommitId::new(parent.commit.as_str());
                if parent.branches.contains(&label) && !writes.labelled.contains(&commit) {
                    writes.labelled.push(commit);
                }
            }
        }
        for commit in writes
            .recorded
            .clone()
            .into_iter()
            .chain(published.cloned())
        {
            if writes.held.contains(&commit) || repo.resolve_commit(commit.as_str()).is_err() {
                continue;
            }
            let rewrites = repo.rewrites_of(commit.as_str())?;
            writes.held.push(commit);
            writes.push_held(rewrites);
        }
        for change in writes.changes.clone() {
            writes.push_held(repo.visible_commits_of_change(&change)?);
        }
        Ok(writes)
    }

    fn push_held(&mut self, commits: Vec<CommitId>) {
        for commit in commits {
            if !self.held.contains(&commit) {
                self.held.push(commit);
            }
        }
    }

    /// The earlier writes this repository holds.
    pub fn commits(&self) -> &[CommitId] {
        &self.held
    }

    pub fn contains(&self, commit: &CommitId) -> bool {
        self.held.contains(commit)
            || self.recorded.contains(commit)
            || self.labelled.contains(commit)
    }

    /// Which of a release's `parents` are fast-forward carriers: earlier
    /// writes of the same name, never members, in parent order.
    ///
    /// A parent of a visible release is visible, so a copy that keeps a
    /// recorded change id is among them.
    ///
    /// A push of a release that does not descend from its published position
    /// is made a fast-forward by giving it that position as one more parent,
    /// so the release then carries an older write of itself. Read as a
    /// member, the carrier accumulates: every later write would keep it, and
    /// each push would add one more. So every write that composes from a
    /// release's parents leaves its carriers out, and the plan and `members`
    /// label them. No other parent is a carrier, however it is named: a write
    /// recorded under another release name is that release's, and here it is
    /// a member like any other.
    pub fn carriers(&self, parents: &[CommitId]) -> Vec<CommitId> {
        parents
            .iter()
            .filter(|parent| self.contains(parent))
            .cloned()
            .collect()
    }

    /// A release's `parents` split into its carriers ([`Self::carriers`])
    /// and its members, the rest, each in parent order.
    pub fn partition(&self, parents: &[CommitId]) -> (Vec<CommitId>, Vec<CommitId>) {
        parents
            .iter()
            .cloned()
            .partition(|parent| self.contains(parent))
    }

    /// [`Self::carriers`] of a parent set a ledger event recorded, whose
    /// commits may since have been abandoned: a hidden commit this repository
    /// holds is a carrier when its change id is a recorded write's.
    pub fn recorded_carriers(
        &self,
        repo: &Repo,
        parents: &[CommitId],
    ) -> Result<Vec<CommitId>, jj::JjError> {
        let mut found = Vec::new();
        for parent in parents {
            let carrier = self.contains(parent)
                || (repo.resolve_commit(parent.as_str()).is_ok() && {
                    let change = repo.change_id_of(parent.as_str())?;
                    self.changes.iter().any(|known| known == change.short())
                });
            if carrier {
                found.push(parent.clone());
            }
        }
        Ok(found)
    }
}

/// The parent set a cut or edit event records for `write`.
///
/// That is its `parents`, or for an event an older knives wrote without them,
/// the write's own parents where this repository holds it and the event's
/// evidence after the write where it does not, with no branch recorded for
/// any.
fn recorded_parent_set(
    repo: &Repo,
    (event, write): (&Entry, &RecordedWrite),
) -> Result<Vec<RecordedParent>, jj::JjError> {
    if !event.parents.is_empty() {
        return Ok(event.parents.clone());
    }
    let commits = if repo.resolve_commit(write.commit.as_str()).is_ok() {
        repo.parent_commits(write.commit.as_str())?
    } else {
        event
            .evidence
            .iter()
            .skip(1)
            .map(|commit| CommitId::new(commit.as_str()))
            .collect()
    };
    Ok(commits
        .into_iter()
        .map(|commit| RecordedParent {
            commit: commit.as_str().to_owned(),
            branches: Vec::new(),
        })
        .collect())
}

/// A recorded write's members: its recorded parent set, carriers left out.
///
/// The set is [`recorded_parent_set`]'s, and a carrier is one
/// [`EarlierWrites::recorded_carriers`] finds or one recorded under the label
/// `adopt` gives a carrier ([`carrier_label`]).
pub fn recorded_members(
    repo: &Repo,
    (release, event, write): (&str, &Entry, &RecordedWrite),
    writes: &EarlierWrites,
) -> Result<Vec<RecordedParent>, jj::JjError> {
    let recorded = recorded_parent_set(repo, (event, write))?;
    let commits: Vec<CommitId> = recorded
        .iter()
        .map(|parent| CommitId::new(parent.commit.as_str()))
        .collect();
    let carriers = writes.recorded_carriers(repo, &commits)?;
    let label = carrier_label(release);
    Ok(recorded
        .into_iter()
        .filter(|parent| {
            !carriers.contains(&CommitId::new(parent.commit.as_str()))
                && !parent.branches.contains(&label)
        })
        .collect())
}

/// A recorded write of a release whose members match a release's
/// ([`matched_write`]).
#[derive(Debug)]
pub struct MatchedWrite<'a> {
    pub event: &'a Entry,
    pub write: RecordedWrite,
    /// The write's members ([`recorded_members`]).
    pub members: Vec<RecordedParent>,
}

/// The newest recorded write of `release` whose members match `members`.
///
/// `members` are a release's parents with its carriers left out, and the
/// write's are [`recorded_members`]'s ([`parents_match`]). `None` when no
/// write matches: the membership changed outside knives.
pub fn matched_write<'a>(
    repo: &Repo,
    (entries, release): (&'a [Entry], &'a str),
    writes: &EarlierWrites,
    members: &[CommitId],
) -> Result<Option<MatchedWrite<'a>>, jj::JjError> {
    for (event, write) in recorded_write_events(entries, release) {
        let recorded = recorded_members(repo, (release, event, &write), writes)?;
        if parents_match(repo, &recorded, members)? {
            return Ok(Some(MatchedWrite {
                event,
                write,
                members: recorded,
            }));
        }
    }
    Ok(None)
}

/// Whether the write's `recorded` members and `members` match one to one.
///
/// A recorded member matches the commit it names, or failing that a visible
/// rewrite of it with the same change id, which is what jj leaves when a
/// member is reworded or rebased in place. A recorded member this repository
/// does not hold matches by its id alone.
fn parents_match(
    repo: &Repo,
    recorded: &[RecordedParent],
    members: &[CommitId],
) -> Result<bool, jj::JjError> {
    if recorded.len() != members.len() {
        return Ok(false);
    }
    let mut open: Vec<&CommitId> = members.iter().collect();
    let mut rewritten = Vec::new();
    for parent in recorded {
        match open
            .iter()
            .position(|member| member.as_str() == parent.commit)
        {
            Some(at) => {
                open.swap_remove(at);
            }
            None => rewritten.push(parent),
        }
    }
    for parent in rewritten {
        if repo.resolve_commit(&parent.commit).is_err() {
            return Ok(false);
        }
        let change = repo.change_id_of(&parent.commit)?;
        let mut found = None;
        for (at, member) in open.iter().enumerate() {
            if repo.change_id_of(member.as_str())? == change {
                found = Some(at);
                break;
            }
        }
        let Some(at) = found else {
            return Ok(false);
        };
        open.swap_remove(at);
    }
    Ok(true)
}

/// The branch label a carrier of `release` ([`EarlierWrites::carriers`])
/// takes where a recorded parent set names it, so no reader takes it for a
/// member's branch.
pub fn carrier_label(release: &str) -> String {
    format!("{release} earlier write (carrier, not a member)")
}

/// The newest structural cut event of the series `entry` selects.
///
/// It is the previous cut that a cut of that series is held against
/// ([`CompositionBaseline`]). An entry without named series reads every cut
/// event, as it always has; with them, another series' cut never stands in
/// as this series' previous composition.
pub fn last_series_cut(entries: &[Entry], entry: &RepoEntry) -> Option<RecordedCut> {
    if entry.series.is_empty() {
        return last_recorded_cut(entries, None);
    }
    let scheme = entry.release_scheme();
    let withdrawn = Withdrawn::read(entries);
    entries
        .iter()
        .rev()
        .filter(|recorded| {
            recorded
                .subject
                .as_deref()
                .is_some_and(|subject| scheme.owns(subject))
        })
        .filter_map(cut_record)
        .find(|cut| !withdrawn.withdraws(&cut.name, &cut.commit))
}

/// What a cut's composition gate holds the cut subject against.
///
/// It is two records: the series' previous cut ([`last_series_cut`]), and
/// the newest recorded write of the newest release the ledger records in the
/// series ([`newest_release_write`]). The cut's event restates a member that
/// a `drop` removed after that cut; the newest write holds a member that an
/// `include` added after it.
#[derive(Debug)]
pub struct CompositionBaseline {
    pub previous_cut: Option<RecordedCut>,
    pub newest_write: Option<RecordedCut>,
}

impl CompositionBaseline {
    /// The baseline of a cut of the series `entry` selects. `except` is the
    /// name that `cut NAME --at` records: no write of it is its own
    /// baseline.
    pub fn read(entries: &[Entry], entry: &RepoEntry, except: Option<&str>) -> Self {
        Self {
            previous_cut: last_series_cut(entries, entry),
            newest_write: newest_release_write(entries, &entry.release_scheme(), except),
        }
    }

    /// Each member that either record holds, once: the previous cut's in its
    /// order, then each one that only the newest write holds.
    pub fn members(&self) -> impl Iterator<Item = &CommitId> {
        let cut = self
            .previous_cut
            .as_ref()
            .map_or(&[][..], |cut| cut.members.as_slice());
        cut.iter().chain(
            self.newest_write
                .iter()
                .flat_map(|write| &write.members)
                .filter(move |member| !cut.contains(member)),
        )
    }
}

/// How one release reads: its members and its carriers ([`read_release`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseReading {
    /// Each member, in the order a flat cut of them lists its parents.
    pub members: Vec<CommitId>,
    /// The parents that are earlier writes of the release's own name
    /// ([`EarlierWrites::carriers`]): never a member.
    pub carriers: Vec<CommitId>,
}

/// What `commit` reads as: its parents, the carriers among them left out
/// ([`EarlierWrites::partition`]). `writes` are the earlier writes of the
/// commit's own name.
pub fn read_release(
    repo: &Repo,
    (writes, commit): (&EarlierWrites, &CommitId),
) -> Result<ReleaseReading, jj::JjError> {
    let (carriers, members) = writes.partition(&repo.parent_commits(commit.as_str())?);
    Ok(ReleaseReading { members, carriers })
}

#[cfg(test)]
mod carrier_tests {
    use std::path::{Path, PathBuf};

    use super::{CommitId, EarlierWrites, Entry, Kind, RecordedParent, carrier_label};
    use crate::config::test_support::run_jj;
    use crate::jj::Repo;

    const RELEASE: &str = "integration";

    /// A cut or edit event of `subject` that names `commit` first in its
    /// evidence, as both writers record it.
    pub(super) fn event(subject: &str, text: String, commit: &CommitId) -> Entry {
        Entry {
            ts: "2026-11-15T00:00:00Z".to_owned(),
            owner: "test".to_owned(),
            email: None,
            subject: Some(subject.to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text,
            evidence: vec![commit.as_str().to_owned(), "member0000000".to_owned()],
            anchor: None,
            anchor_ref: None,
            pr: None,
            parents: vec![RecordedParent {
                commit: "member0000000".to_owned(),
                branches: vec!["feat/member".to_owned()],
            }],
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

    /// A repository holding one commit on the root per name, each bookmarked
    /// by its name, with the working copy moved off them.
    fn repository(directory: &Path, names: &[&str]) -> PathBuf {
        let path = directory.join("repo");
        run_jj(
            directory,
            &["git", "init", path.to_str().expect("utf-8 path")],
        );
        for name in names {
            run_jj(&path, &["new", "-r", "root()", "-m", name]);
            run_jj(&path, &["bookmark", "create", name, "-r", "@"]);
        }
        run_jj(&path, &["new", "-r", "root()"]);
        path
    }

    pub(super) fn at(path: &Path, revision: &str) -> CommitId {
        Repo::open(path)
            .expect("open test repository")
            .resolve_commit(revision)
            .expect("resolve a fixture commit")
    }

    #[test]
    fn earlier_writes_of_the_name_its_published_commit_and_a_rewrite_are_carriers() {
        // Given: a recorded cut and two recorded edits of the release, the
        // second edit since reworded (its recorded id hidden, its rewrite
        // visible), and the publish remote at a fourth commit no event names.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(
            directory.path(),
            &["member", "cut", "edit", "published", "reworded"],
        );
        let (member, cut, edit) = (at(&path, "member"), at(&path, "cut"), at(&path, "edit"));
        let (published, recorded) = (at(&path, "published"), at(&path, "reworded"));
        let entries = [
            event(
                RELEASE,
                format!(
                    "cut {RELEASE} as {} with 1 parent(s): feat/member",
                    cut.short()
                ),
                &cut,
            ),
            event(
                RELEASE,
                format!(
                    "edited {RELEASE} as {} with 1 parent(s): included x",
                    edit.short()
                ),
                &edit,
            ),
            event(
                RELEASE,
                format!(
                    "edited {RELEASE} as {} with 1 parent(s): included y",
                    recorded.short()
                ),
                &recorded,
            ),
        ];
        run_jj(
            &path,
            &["describe", "-r", "reworded", "-m", "reworded again"],
        );
        let rewrite = at(&path, "reworded");
        assert_ne!(rewrite, recorded, "the describe rewrote the recorded edit");
        let parents = [
            member,
            cut.clone(),
            edit.clone(),
            published.clone(),
            rewrite.clone(),
        ];
        let opened = Repo::open(&path).expect("open test repository");

        // When: the release's parents are read for carriers.
        let found = EarlierWrites::read(&opened, RELEASE, &entries, Some(&published))
            .expect("read earlier writes")
            .carriers(&parents);

        // Then: the cut, the edit, the published commit and the rewrite of
        // the reworded edit are each one, in the release's parent order; the
        // member is not.
        assert_eq!(found, [cut, edit, published, rewrite]);
    }

    #[test]
    fn a_copy_that_keeps_a_recorded_change_id_is_a_carrier_where_the_write_is_absent() {
        // Given: a cut this repository never held, recorded with the change
        // id of a commit it does hold (a fast-forward copy of the cut, pushed
        // from another clone), and a member no event names.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path(), &["member", "copy"]);
        let (member, copy) = (at(&path, "member"), at(&path, "copy"));
        let opened = Repo::open(&path).expect("open test repository");
        let change = opened.change_id_of(copy.as_str()).expect("read change id");
        let absent = CommitId::new("f".repeat(40));
        assert!(opened.resolve_commit(absent.as_str()).is_err());
        let entries = [event(
            RELEASE,
            format!(
                "cut {RELEASE} as {} (change {}) with 1 parent(s): feat/member",
                absent.short(),
                change.short()
            ),
            &absent,
        )];

        // When: the release's parents are read for carriers, the publish
        // remote holding no ref of the name.
        let found = EarlierWrites::read(&opened, RELEASE, &entries, None)
            .expect("read earlier writes")
            .carriers(&[member, copy.clone()]);

        // Then: the copy is a carrier by its change id; the member is not.
        assert_eq!(found, [copy]);
    }

    #[test]
    fn a_member_a_bare_commit_and_another_releases_write_are_not_carriers() {
        // Given: a write recorded under another release name, and a member
        // and a bare commit no event names as a write.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path(), &["feat/member", "bare", "other"]);
        let (member, bare, other) = (
            at(&path, "feat/member"),
            at(&path, "bare"),
            at(&path, "other"),
        );
        run_jj(&path, &["bookmark", "delete", "bare"]);
        let entries = [event(
            "release/2026-08-04",
            format!(
                "cut release/2026-08-04 as {} with 1 parent(s): feat/member",
                other.short()
            ),
            &other,
        )];
        let parents = [member, bare, other];
        let opened = Repo::open(&path).expect("open test repository");

        // When: the release's parents are read for carriers, its publish
        // remote holding no ref of the name.
        let found = EarlierWrites::read(&opened, RELEASE, &entries, None)
            .expect("read earlier writes")
            .carriers(&parents);

        // Then: none of them is a carrier.
        assert_eq!(found, Vec::<CommitId>::new());
    }

    #[test]
    fn a_carrier_is_labelled_as_an_earlier_write_of_its_release() {
        assert_eq!(
            carrier_label(RELEASE),
            "integration earlier write (carrier, not a member)"
        );
    }

    #[test]
    fn a_parent_recorded_with_the_carrier_label_is_a_carrier_and_one_recorded_as_a_member_is_not() {
        // Given: an adopt event of the release whose parent set lists a
        // pushed resolution with the carrier label, and no event recording
        // that resolution as a write.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path(), &["member", "adopted", "pushed"]);
        let (member, adopted, pushed) = (
            at(&path, "member"),
            at(&path, "adopted"),
            at(&path, "pushed"),
        );
        let mut adopt = event(
            RELEASE,
            format!(
                "edited {RELEASE} as {} with 2 parent(s): adopted: a resolution",
                adopted.short()
            ),
            &adopted,
        );
        adopt.parents = vec![
            RecordedParent {
                commit: member.as_str().to_owned(),
                branches: vec!["feat/member".to_owned()],
            },
            RecordedParent {
                commit: pushed.as_str().to_owned(),
                branches: vec![carrier_label(RELEASE)],
            },
        ];
        let opened = Repo::open(&path).expect("open test repository");
        let parents = [member, pushed.clone()];

        // When: the release's parents are read for carriers.
        let found = EarlierWrites::read(&opened, RELEASE, std::slice::from_ref(&adopt), None)
            .expect("read earlier writes")
            .carriers(&parents);

        // Then: the labelled parent is a carrier.
        assert_eq!(found, vec![pushed]);

        // When (control): the same parent is recorded under a member's branch.
        if let Some(labelled) = adopt.parents.get_mut(1) {
            labelled.branches = vec!["feat/pushed".to_owned()];
        }
        let found = EarlierWrites::read(&opened, RELEASE, std::slice::from_ref(&adopt), None)
            .expect("read earlier writes")
            .carriers(&parents);

        // Then: it is a member.
        assert_eq!(found, Vec::<CommitId>::new());
    }
}

#[cfg(test)]
mod unchecked_mark_tests {
    use super::{
        CommitId, RecordedWrite, RecordedWriteKind, carrier_tests::event, recorded_write_events,
        unchecked_mark,
    };
    use crate::commands::release::{InHandWrite, NoMatch, Unchecked, unchecked_suffix};

    /// An edit event of `integration` whose text says `delta`, then its
    /// parents, then `suffix`, as `record_edit_event` writes one.
    fn edited(delta: &str, suffix: &str) -> crate::ledger::Entry {
        let commit = CommitId::new("a".repeat(40));
        event(
            "integration",
            format!(
                "edited integration as {} (change kkkkkkkkkkkk) with 1 parent(s): {delta}; \
                 parents: feat/x@bbbbbbbbbbbb{suffix}",
                commit.short()
            ),
            &commit,
        )
    }

    #[test]
    fn a_reason_holding_the_delimiter_is_no_mark_and_a_written_mark_is() {
        // Given: an include and an adopt whose caller's reason holds the
        // delimiter, with a reason a write marks and with one it does not,
        // and an event that ends with the delimiter and no written reason.
        let unmarked = [
            edited("included feat/x (x; unchecked: y)", ""),
            edited("included feat/x (renumber; unchecked: W absent)", ""),
            edited("included feat/x (renumber; unchecked: no record)", ""),
            edited(
                "adopted: replaces the recorded cut 0123456789ab; why: renumber; unchecked: \
                 no match",
                "",
            ),
            edited("included feat/x", "; unchecked: y"),
            // An `--allow-loss` reason sits after the delta, before the
            // parents, so one that ends with a mark is no mark.
            edited(
                "dropped feat/d: x; lost: CHANGELOG.md (feat/a: 1 line(s)); why lost: r; \
                 unchecked: no record",
                "",
            ),
        ];

        // Then: none reads as marked.
        for entry in &unmarked {
            assert_eq!(unchecked_mark(entry), None, "{}", entry.text);
        }

        // Given: the same include written over each release knives could not
        // check, a name the ledger records no write of included, its mark
        // written as every write writes it.
        let write = RecordedWrite {
            kind: RecordedWriteKind::Cut,
            commit: CommitId::new("c".repeat(40)),
        };
        let cases = [
            (
                InHandWrite::Unchecked(Unchecked::WriteAbsent(write.clone())),
                "W absent",
            ),
            (
                InHandWrite::Unchecked(Unchecked::NoMatch(NoMatch::Members)),
                "no match",
            ),
            (InHandWrite::NoRecord, "no record"),
            (
                InHandWrite::Unchecked(Unchecked::Inherited {
                    write: write.clone(),
                    reason: "no match".to_owned(),
                }),
                "no match",
            ),
            (
                InHandWrite::Unchecked(Unchecked::Inherited {
                    write,
                    reason: "no record".to_owned(),
                }),
                "no record",
            ),
        ];
        for (standing, reason) in cases {
            let suffix = unchecked_suffix(&standing);
            let marked = edited("included feat/x (x; unchecked: y)", &suffix);

            // Then: it reads as marked, with the reason written.
            assert_eq!(unchecked_mark(&marked), Some(reason), "{}", marked.text);
        }

        // And: a write over a release knives checked carries no mark.
        let checked = edited("included feat/x", &unchecked_suffix(&InHandWrite::Recorded));
        assert_eq!(unchecked_mark(&checked), None, "{}", checked.text);

        // And: an `--allow-loss` reason that holds `; parents: x` leaves
        // knives' own parents the last `; parents: `, which every reader of
        // the text takes; the event still reads as an edit of its commit.
        let loss = edited(
            "dropped feat/d: x; lost: CHANGELOG.md (feat/a: 1 line(s)); why lost: see; parents: x",
            "",
        );
        assert_eq!(
            loss.text
                .rsplit_once("; parents: ")
                .map(|(_, parents)| parents),
            Some("feat/x@bbbbbbbbbbbb")
        );
        assert_eq!(unchecked_mark(&loss), None, "{}", loss.text);
        let write = recorded_write_events(std::slice::from_ref(&loss), "integration")
            .map(|(_, write)| write)
            .next();
        assert_eq!(
            write,
            Some(RecordedWrite {
                kind: RecordedWriteKind::Edit,
                commit: CommitId::new("a".repeat(40)),
            })
        );
    }
}

#[cfg(test)]
mod withdrawn_tests {
    use super::{
        CommitId, Entry, RecordedParent, RecordedWriteKind, Withdrawn, carrier_label,
        carrier_tests::event, last_recorded_cut, last_recorded_parents, recorded_writes,
        withdrawal_text,
    };

    const RELEASE: &str = "integration";

    fn commit(digit: char) -> CommitId {
        CommitId::new(digit.to_string().repeat(40))
    }

    /// A cut of [`RELEASE`] at `created`, whose parents are `member0000000`
    /// and, where given, `carrier`, labelled as a carrier.
    fn cut(created: &CommitId, carrier: Option<&CommitId>) -> Entry {
        let mut entry = event(
            RELEASE,
            format!(
                "cut {RELEASE} as {} with 1 parent(s): feat/member",
                created.short()
            ),
            created,
        );
        if let Some(carrier) = carrier {
            entry.evidence.push(carrier.as_str().to_owned());
            entry.parents.push(RecordedParent {
                commit: carrier.as_str().to_owned(),
                branches: vec![carrier_label(RELEASE)],
            });
        }
        entry
    }

    /// The withdrawal of `w`, a cut of [`RELEASE`], that names `p`, the
    /// published commit, with `id` as the withdrawn write's commit.
    fn withdrawal(w: &CommitId, id: &str, p: &CommitId) -> Entry {
        let mut entry = event(
            RELEASE,
            withdrawal_text(RELEASE, (RecordedWriteKind::Cut, w), ("origin", p)),
            w,
        );
        entry.evidence = vec![id.to_owned(), p.as_str().to_owned()];
        entry.parents.clear();
        entry
    }

    #[test]
    fn a_withdrawn_write_is_never_written_and_a_labelled_carrier_is_no_member() {
        // Given: a cut A, a cut W, and a withdrawal of W that names P, the
        // published commit; then a cut of P with A as a labelled carrier.
        let (a, w, p) = (commit('a'), commit('b'), commit('c'));
        let before = [cut(&a, None), cut(&w, None), withdrawal(&w, w.as_str(), &p)];

        // Then: W is no write and no previous cut; A is.
        assert_eq!(
            recorded_writes(&before, RELEASE)
                .map(|write| write.commit)
                .collect::<Vec<_>>(),
            std::slice::from_ref(&a)
        );
        assert_eq!(
            last_recorded_cut(&before, Some(RELEASE)).map(|cut| cut.commit),
            Some(a.clone())
        );
        assert_eq!(
            last_recorded_parents(&before, RELEASE),
            cut(&a, None).parents.as_slice()
        );

        // And: once P is cut with A as its carrier, P is the previous cut,
        // and its members leave A out.
        let mut after = before.to_vec();
        after.push(cut(&p, Some(&a)));
        let recorded = last_recorded_cut(&after, None).expect("a recorded cut");
        assert_eq!(recorded.commit, p);
        assert_eq!(recorded.members, [CommitId::new("member0000000")]);

        // And (control): without the withdrawal, W is the newest write.
        let unwithdrawn = [cut(&a, None), cut(&w, None)];
        assert_eq!(
            recorded_writes(&unwithdrawn, RELEASE)
                .next()
                .map(|write| write.commit),
            Some(w.clone())
        );

        // And: a withdrawal whose id is shorter than a short commit
        // withdraws nothing; one of 12 characters withdraws its write.
        for (length, withdrawn) in [(11, false), (12, true)] {
            let id = w.as_str().get(..length).expect("a short id");
            let entries = [cut(&a, None), cut(&w, None), withdrawal(&w, id, &p)];
            assert_eq!(
                Withdrawn::read(&entries).withdraws(RELEASE, &w),
                withdrawn,
                "{id}"
            );
        }
    }
}

#[cfg(test)]
mod adopted_fix_tests {
    use std::collections::HashSet;

    use super::{
        AdoptedFixFiles, CommitId, TEXT_PATH_PIECES, adopted_fix_replaces, adopted_fix_text,
        carrier_tests::event,
    };
    use crate::ledger::{Entry, FixChange, FixFile};

    /// The files that `entry` names as its fix by hand, as an `adopt` event
    /// of `release` ([`adopted_fix_text`]).
    fn named_files(entry: &Entry, release: &str) -> AdoptedFixFiles {
        adopted_fix_text(entry, release).files()
    }

    /// An `adopt` event of `integration` whose delta after its baseline says
    /// `parts`, as `adopt` writes one.
    fn adopted(parts: &str) -> Entry {
        let commit = CommitId::new("a".repeat(40));
        event(
            "integration",
            format!(
                "edited integration as {} (change kkkkkkkkkkkk) with 2 parent(s): adopted: \
                 replaces the recorded cut 0123456789ab; measured against the recorded cut \
                 0123456789ab; {parts}; parents: feat/x@bbbbbbbbbbbb, feat/y@cccccccccccc",
                commit.short()
            ),
            &commit,
        )
    }

    #[test]
    fn an_adopt_event_names_the_files_of_its_fix_and_no_other_event_does() {
        // Given: adopts that resolved files by hand, changed files beyond the
        // baseline's conflicts with a reason that holds both separators, or
        // with no reason, or both; and an adopt that changed no file.
        let cases = [
            (
                "resolved by hand: a.txt, dir/b.txt",
                vec!["a.txt", "a.txt, dir/b.txt", "dir/b.txt"],
            ),
            (
                "changed beyond the baseline's conflicts: fix.txt (why: a fix, by hand; again)",
                vec!["fix.txt"],
            ),
            (
                "resolved by hand: a.txt; changed beyond the baseline's conflicts: c.txt, d.txt \
                 (why: both); carriers: 0123456789ab",
                vec!["a.txt", "c.txt", "c.txt, d.txt", "d.txt"],
            ),
            (
                "changed beyond the baseline's conflicts: e.txt; carriers: 0123456789ab",
                vec!["e.txt"],
            ),
            (
                "resolved by hand: f.txt; why: a fix, g.txt; carriers: 0123456789ab",
                vec!["f.txt"],
            ),
            ("why: nothing changed", Vec::new()),
        ];

        for (parts, files) in cases {
            let entry = adopted(parts);

            // Then: each names every path that its text may name as a file
            // of its fix, as two files joined by `, ` may be one path that
            // holds it, and never a word of its reason, its carriers or its
            // parents; and it names the write it replaced exactly when it
            // names a file.
            let named = named_files(&entry, "integration");
            assert_eq!(named.files, files, "{parts}");
            assert_eq!(named.from_text, !files.is_empty(), "{parts}");
            assert_eq!(
                adopted_fix_replaces(&entry, "integration").is_some(),
                !files.is_empty(),
                "{parts}"
            );
            // And: another release's name reads no fix in it.
            assert_eq!(
                named_files(&entry, "release/2026-08-04"),
                AdoptedFixFiles::default()
            );
        }
    }

    #[test]
    fn an_adopt_events_list_names_each_path_whole_where_its_text_names_pieces() {
        // Given: an adopt that resolved `x, y.txt` by hand and changed
        // `x (why: y.txt` beyond its baseline's conflicts, whose text names
        // both; once with its `fix_files` list, and once without it, as a
        // knives from before the list writes the event.
        let older = adopted(
            "resolved by hand: x, y.txt; changed beyond the baseline's conflicts: x (why: y.txt \
             (why: a fix)",
        );
        let listed = Entry {
            fix_files: vec![
                FixFile {
                    path: "x, y.txt".to_owned(),
                    change: FixChange::Resolved,
                },
                FixFile {
                    path: "x (why: y.txt".to_owned(),
                    change: FixChange::Beyond,
                },
            ],
            ..older.clone()
        };

        // When: each event's files are read.
        let whole = named_files(&listed, "integration");
        let pieces = named_files(&older, "integration");

        // Then: the list gives each path whole; the text gives every path
        // that it may name, each whole path among them, and says that it
        // read them from the text.
        assert_eq!(whole.files, ["x, y.txt", "x (why: y.txt"]);
        assert!(!whole.from_text);
        assert_eq!(pieces.files, ["x", "x, y.txt", "y.txt", "x (why: y.txt"]);
        assert!(pieces.from_text);
        assert_eq!(
            adopted_fix_replaces(&listed, "integration"),
            adopted_fix_replaces(&older, "integration")
        );
    }

    #[test]
    fn an_older_adopt_events_long_list_reads_at_most_sixteen_paths_for_each_of_its_pieces() {
        // Given: an adopt of 3,000 files, of a path that joins 16 pieces with
        // `, ` and of one that joins 17, whose text names them in one list,
        // as a knives from before the `fix_files` list writes it.
        let many: Vec<String> = (0..3000).map(|file| format!("f{file:04}.txt")).collect();
        let joined = |prefix: char, pieces: usize| {
            (1..=pieces)
                .map(|piece| format!("{prefix}{piece:02}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (sixteen, seventeen) = (joined('p', 16), joined('q', 17));
        let entry = adopted(&format!(
            "changed beyond the baseline's conflicts: {}, {sixteen}, {seventeen} (why: a fix)",
            many.join(", ")
        ));
        let pieces = many.len() + 16 + 17;

        // When: its files are read.
        let named = named_files(&entry, "integration");

        // Then: they are at most 16 paths for each piece of the list, each
        // once, and they hold each file that joins 16 pieces or fewer, but
        // not the path that joins 17.
        assert!(named.from_text);
        assert!(
            named.files.len() <= pieces * TEXT_PATH_PIECES,
            "{} paths",
            named.files.len()
        );
        let read: HashSet<&str> = named.files.iter().map(String::as_str).collect();
        assert_eq!(read.len(), named.files.len());
        for file in many.iter().chain(std::iter::once(&sixteen)) {
            assert!(read.contains(file.as_str()), "{file}");
        }
        assert!(!read.contains(seventeen.as_str()));
    }

    #[test]
    fn an_older_adopt_events_lists_end_at_its_last_parents_and_its_last_reason() {
        // Given: a resolved list that no other part follows, a path that
        // holds `; parents: ` among its files, as every event ends at its
        // last `; parents: `; and a text with no resolved list, whose list
        // changed beyond the baseline's conflicts reads up to its last
        // ` (why: `, a path that holds `; why: ` among its files.
        for (parts, path) in [
            ("resolved by hand: notes/a, b.txt", "notes/a, b.txt"),
            (
                "resolved by hand: notes/a; parents: b.md",
                "notes/a; parents: b.md",
            ),
            (
                "changed beyond the baseline's conflicts: notes/a; why: b.md (why: kept the fix); \
                 carriers: 0123456789ab",
                "notes/a; why: b.md",
            ),
        ] {
            // When: its files are read.
            let entry = adopted(parts);

            // Then: the path is among them.
            assert!(
                named_files(&entry, "integration")
                    .files
                    .iter()
                    .any(|file| file == path),
                "{parts}"
            );
        }
    }
}

#[cfg(test)]
mod reading_tests {
    use std::path::{Path, PathBuf};

    use super::{CommitId, EarlierWrites, ReleaseReading, carrier_tests::at, read_release};
    use crate::config::test_support::run_jj;
    use crate::jj::Repo;

    /// The first cut of series `old`: the release that the chain the test
    /// reads is made on.
    const PREVIOUS: &str = "release/old/2026-08-04";

    /// A commit on `parents`, described and bookmarked as `name`.
    fn commit(path: &Path, parents: &[&str], name: &str) {
        let mut args = vec!["new"];
        for parent in parents {
            args.extend(["-r", *parent]);
        }
        args.extend(["-m", name]);
        run_jj(path, &args);
        run_jj(path, &["bookmark", "create", name, "-r", "@"]);
    }

    /// A repository with the series base `old`, two branches on it, their
    /// flat first cut [`PREVIOUS`], three branches on that cut (`del`, `m6`
    /// and `m5`), and the chain `chain` made from them by hand, which is
    /// `merge(merge(merge(PREVIOUS, del), m6), m5)` through `inner1` and
    /// `inner2`.
    fn repository(directory: &Path) -> PathBuf {
        let path = directory.join("repo");
        run_jj(
            directory,
            &["git", "init", path.to_str().expect("utf-8 path")],
        );
        commit(&path, &["root()"], "old");
        commit(&path, &["old"], "fix/a");
        commit(&path, &["old"], "fix/b");
        commit(&path, &["fix/a", "fix/b"], PREVIOUS);
        for branch in ["del", "m6", "m5"] {
            commit(&path, &[PREVIOUS], branch);
        }
        commit(&path, &[PREVIOUS, "del"], "inner1");
        commit(&path, &["inner1", "m6"], "inner2");
        commit(&path, &["inner2", "m5"], "chain");
        run_jj(&path, &["new", "-r", "root()"]);
        path
    }

    fn commits(path: &Path, revisions: &[&str]) -> Vec<CommitId> {
        revisions
            .iter()
            .map(|revision| at(path, revision))
            .collect()
    }

    /// `revision` as it reads with no ledger.
    fn read(path: &Path, revision: &str) -> ReleaseReading {
        let repo = Repo::open(path).expect("open test repository");
        let writes = EarlierWrites::read(&repo, revision, &[], None).expect("read earlier writes");
        read_release(&repo, (&writes, &at(path, revision))).expect("read the release")
    }

    #[test]
    fn outside_a_stacked_series_a_release_reads_its_own_parents() {
        // Given: the chain.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());

        // When: it is read.
        let reading = read(&path, "chain");

        // Then: its own parents are its members.
        assert_eq!(
            reading,
            ReleaseReading {
                members: commits(&path, &["inner2", "m5"]),
                carriers: Vec::new(),
            }
        );
    }
}
