//! Release-domain rules shared by command verbs and reports.
//!
//! This module owns facts about release names, recorded cuts, and consumer pins.
//! Commands gather their I/O and render their answers around these rules.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, Finding, FindingKind, Subject};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, is_our_release, is_release_name,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, Kind, RecordedParent};
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

/// Every commit one of our release refs names, with the refs naming it.
///
/// Local refs and refs on the publish remote count; `origin` release-shaped
/// refs in a split-release repository and upstream's own cuts do not, the same
/// trust boundary [`is_our_release`] draws everywhere else.
pub fn release_refs_by_commit(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> BTreeMap<CommitId, Vec<BookmarkRef>> {
    let mut by_commit: BTreeMap<CommitId, Vec<BookmarkRef>> = BTreeMap::new();
    for (reference, commit) in tips {
        if is_our_release(reference, scheme, publish_remote) {
            by_commit
                .entry(commit.clone())
                .or_default()
                .push(reference.clone());
        }
    }
    by_commit
}

/// What every stacked-history check in one run shares: the repository, the
/// trunk positions to measure past, which commits our release refs name, and
/// what the release already carries.
#[derive(Clone, Copy)]
pub struct StackedHistoryContext<'a> {
    pub repo: &'a Repo,
    /// Every known trunk position ([`trunk_positions`]): a merge counts only
    /// when it joins lines none of them reaches.
    pub trunks: &'a [CommitId],
    pub releases: &'a BTreeMap<CommitId, Vec<BookmarkRef>>,
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

/// `commits`, then `extra` when there is one: the trunk positions a
/// branch's history is measured past ([`StackedHistoryContext::trunks`]),
/// with a release it may stack on too.
pub fn with_commit<'a>(commits: &'a [CommitId], extra: Option<&CommitId>) -> Cow<'a, [CommitId]> {
    extra.map_or(Cow::Borrowed(commits), |extra| {
        Cow::Owned(
            commits
                .iter()
                .chain(std::iter::once(extra))
                .cloned()
                .collect(),
        )
    })
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
/// and, for each that one of our release refs names, which release it is. A
/// merge no release ref names may be upstream's own, visible only because
/// every local view of the trunk is behind the branch's base; the finding says
/// so, since the local repository cannot exclude it without fetching.
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
            context.releases.get(merge).map(|refs| {
                let names: Vec<String> = refs.iter().map(ToString::to_string).collect();
                format!("{} ({})", names.join(", "), merge.short())
            })
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
    for trunk in trunks {
        if repo.is_ancestor(commit, trunk)? {
            return Ok(true);
        }
    }
    Ok(false)
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
/// advance or rebase event that wrote one. Empty when nothing was recorded.
pub fn last_recorded_parents<'a>(entries: &'a [Entry], release: &str) -> &'a [RecordedParent] {
    entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(release))
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

/// The newest structural cut event, optionally scoped to a release name.
///
/// Its members are its evidence after the commit, less the previous release
/// its parent set labels ([`previous_release_label`], [`event_members`]). An
/// event whose evidence names nothing after the commit is no cut; one that
/// names only the previous release is a cut with no member, the start of a
/// stacked cut.
pub fn last_recorded_cut(entries: &[Entry], subject: Option<&str>) -> Option<RecordedCut> {
    entries.iter().rev().find_map(|entry| {
        let entry_subject = entry.subject.as_deref()?;
        if subject.is_some_and(|subject| subject != entry_subject)
            || entry.kind != Kind::Event
            || !entry.text.starts_with(&format!("cut {entry_subject} as "))
        {
            return None;
        }
        let (commit, members) = entry.evidence.split_first()?;
        if members.is_empty() {
            return None;
        }
        Some(RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: entry_subject.to_owned(),
            commit: CommitId::new(commit.as_str()),
            members: event_members(entry, labels_previous),
        })
    })
}

/// The members a cut or edit event records: its evidence after the commit,
/// less each commit that its parent set records under a parent `skip`
/// takes, such as the previous release's label or a carrier's. The one
/// reading of an event's members that [`last_recorded_cut`] and
/// [`newest_release_write`] share, each with its own `skip`.
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
    entries.iter().rev().filter_map(move |entry| {
        if entry.kind != Kind::Event || entry.subject.as_deref() != Some(release) {
            return None;
        }
        if let Some(cut) = last_recorded_cut(std::slice::from_ref(entry), None) {
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
}

/// The newest recorded write of the newest release that `entries` record in
/// the series `scheme`, `except` aside: what a cut is held against beside
/// the series' previous cut ([`CompositionBaseline`]).
///
/// The release is the newest name, in the series' one order
/// ([`ReleaseScheme::cut_key`]), that the ledger records a cut or edit of
/// ([`recorded_release_names`]); under a fixed scheme it is the fixed
/// branch, the one name that the scheme owns. Its newest write is its cut,
/// its newest edit or an `adopt`, whichever the ledger records last
/// ([`recorded_write_events`]). The members are that event's
/// ([`event_members`]), less each parent that its parent set labels as the
/// previous release ([`previous_release_label`]) or as a carrier
/// ([`carrier_label`]). So a member that `include` added after the cut is
/// held, and so is each member of a stacked release, whose cut records none.
pub fn newest_release_write(
    entries: &[Entry],
    scheme: &ReleaseScheme,
    except: Option<&str>,
) -> Option<RecordedCut> {
    let mut names = recorded_release_names(entries, scheme)
        .into_iter()
        .filter(|name| Some(*name) != except);
    let name = match scheme {
        ReleaseScheme::Fixed(_) => names.next()?,
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => {
            names
                .filter_map(|name| scheme.cut_key(name).map(|key| (key, name)))
                .max_by_key(|(key, _)| *key)?
                .1
        }
    };
    let (event, write) = recorded_write_events(entries, name).next()?;
    let carrier = carrier_label(name);
    Some(RecordedCut {
        kind: write.kind,
        name: name.to_owned(),
        commit: write.commit,
        members: event_members(event, |parent| {
            labels_previous(parent) || parent.branches.contains(&carrier)
        }),
    })
}

/// Whether `commit`, or a visible rewrite of it, is `start` or on its
/// first-parent line. A commit this repository does not hold is on no line
/// here.
///
/// In a stacked series each release is built on the one before it, so the
/// newest recorded write must be on this line of a release, or of the
/// commit a cut starts on, for the release to carry everything that write's
/// release carries, every level of the stack below it included
/// ([`crate::commands::release::CompositionDelta::start`]). A rewrite has
/// the write's change id ([`Repo::rewrites_of`]): jj rebases a release over
/// a member rewritten in place, and knives reads that rewrite as the
/// recorded write ([`crate::commands::release::in_hand_write`]), so it
/// stands for the write on the line.
pub fn on_first_parent_line(
    repo: &Repo,
    commit: &CommitId,
    start: &CommitId,
) -> Result<bool, jj::JjError> {
    if repo.resolve_commit(commit.as_str()).is_err() {
        return Ok(false);
    }
    for held in std::iter::once(commit.clone()).chain(repo.rewrites_of(commit.as_str())?) {
        if first_parent_line_holds(repo, &held, start)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `commit` is `start` or on its first-parent line: every commit on
/// the line above `commit` descends from it, so the walk ends at the first
/// commit that does not.
fn first_parent_line_holds(
    repo: &Repo,
    commit: &CommitId,
    start: &CommitId,
) -> Result<bool, jj::JjError> {
    let mut at = start.clone();
    while at != *commit {
        if !repo.is_ancestor(commit, &at)? {
            return Ok(false);
        }
        match repo.parent_commits(at.as_str())?.into_iter().next() {
            Some(parent) => at = parent,
            None => return Ok(false),
        }
    }
    Ok(true)
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
    /// a write, then squashed into it and fast-forwarded onto).
    labelled: Vec<CommitId>,
    /// Every parent a recorded event lists with the previous-release label
    /// ([`previous_release_label`]): the previous release a stacked write
    /// stacks on, neither a carrier nor a member.
    previous: Vec<CommitId>,
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
                } else if labels_previous(parent) && !writes.previous.contains(&commit) {
                    writes.previous.push(commit);
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
    /// and its members, the rest, each in parent order. A parent a recorded
    /// event labels as the previous release is neither.
    pub fn partition(&self, parents: &[CommitId]) -> (Vec<CommitId>, Vec<CommitId>) {
        parents
            .iter()
            .filter(|parent| !self.previous.contains(parent))
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

/// A recorded write's members: its recorded parent set, carriers and the
/// previous release left out.
///
/// The set is [`recorded_parent_set`]'s, and a carrier is one
/// [`EarlierWrites::recorded_carriers`] finds or one recorded under the label
/// `adopt` gives a carrier ([`carrier_label`]). The previous release of a
/// stacked write is recorded under [`previous_release_label`].
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
                && !labels_previous(parent)
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
    entries
        .iter()
        .rev()
        .filter(|recorded| {
            recorded
                .subject
                .as_deref()
                .is_some_and(|subject| scheme.owns(subject))
        })
        .find_map(|recorded| last_recorded_cut(std::slice::from_ref(recorded), None))
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

/// What the label of a stacked release's previous release says after its
/// name ([`previous_release_label`]).
const PREVIOUS_RELEASE_SUFFIX: &str = " previous release (not a member)";

/// The branch label the previous release of a stacked release takes where a
/// recorded parent set names it, as [`carrier_label`] labels a carrier, so no
/// reader takes it for a member's branch.
pub fn previous_release_label(previous: &str) -> String {
    format!("{previous}{PREVIOUS_RELEASE_SUFFIX}")
}

/// The name of the previous release that a recorded parent's branch labels
/// ([`previous_release_label`]), or `None` for any other branch.
fn labelled_previous(branch: &str) -> Option<&str> {
    branch
        .strip_suffix(PREVIOUS_RELEASE_SUFFIX)
        .filter(|name| !name.is_empty())
}

/// Whether a recorded parent's branch is the label [`previous_release_label`]
/// gives the previous release of any release. A branch name holds no space,
/// so no branch reads as one.
pub fn is_previous_release_label(branch: &str) -> bool {
    labelled_previous(branch).is_some()
}

/// Whether a recorded parent carries the label [`previous_release_label`]
/// gives the previous release of any release.
fn labels_previous(parent: &RecordedParent) -> bool {
    parent
        .branches
        .iter()
        .any(|branch| is_previous_release_label(branch))
}

/// The previous release of a release in a stacked series: its first parent,
/// or the first parent of the chain of merges it is, and never a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousRelease {
    /// Its name in the series, as the ledger records it or a release ref
    /// names it.
    pub name: String,
    pub commit: CommitId,
}

impl PreviousRelease {
    /// The release with the label a recorded parent set gives it
    /// ([`previous_release_label`]): what a write's composition lists first.
    pub fn labelled(&self) -> (String, CommitId) {
        (previous_release_label(&self.name), self.commit.clone())
    }
}

/// How one release reads: the previous release it stacks on, its members and
/// its carriers ([`read_release`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseReading {
    /// `None` outside a stacked series, and for the first cut of one.
    pub previous: Option<PreviousRelease>,
    /// Each member, in the order a flat cut of them lists its parents.
    pub members: Vec<CommitId>,
    /// The parents that are earlier writes of the release's own name
    /// ([`EarlierWrites::carriers`]): neither a member nor the previous
    /// release.
    pub carriers: Vec<CommitId>,
}

impl ReleaseReading {
    /// What the release composes, in parent order ([`composition`]).
    pub fn composition(&self) -> Vec<CommitId> {
        composition(self.previous.as_ref(), &self.members).into_owned()
    }
}

/// What a release of `previous` and `members` composes, in parent order: the
/// previous release when there is one, then each member.
pub fn composition<'a>(
    previous: Option<&PreviousRelease>,
    members: &'a [CommitId],
) -> Cow<'a, [CommitId]> {
    previous.map_or(Cow::Borrowed(members), |previous| {
        Cow::Owned(
            std::iter::once(&previous.commit)
                .chain(members)
                .cloned()
                .collect(),
        )
    })
}

/// A commit with one parent between a stacked release and its previous release.
///
/// It is on the release's first-parent line. Only a merge brings members in,
/// so no member explains its change, and [`read_release`] refuses the release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexplainedCommit {
    pub commit: CommitId,
    pub previous: PreviousRelease,
}

impl std::fmt::Display for UnexplainedCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "commit {} has one parent and lies between the release and its previous release \
             {} ({}), so no member explains its change; the release must be merges of its \
             previous release and its members",
            self.commit.short(),
            self.previous.name,
            self.previous.commit.short()
        )
    }
}

/// What a stacked series reads `commit` with: its previous release, its
/// members, its carriers, or the commit that refuses it.
///
/// Outside a stacked series ([`RepoEntry::stacked`]) the members are the
/// commit's parents and the carriers among them are left out
/// ([`EarlierWrites::partition`]). In a stacked series the previous release
/// P is on the commit's first-parent line, the commit itself excluded, and
/// the walk ends before the first commit a trunk position reaches:
///
/// - P is the nearest commit on the line that a cut or edit event of the
///   series names. An event names its write, and the previous release that
///   its parent set labels ([`previous_release_label`]): once the reap has
///   forgotten P's refs, the label is all that names a P cut by hand. With
///   no such event, P is the nearest commit on the line that a release ref
///   of the series names, locally, on the publish remote or on origin
///   ([`Role::Origin`]; [`ReleaseScheme::cut_key`] reads each name, and tags
///   do not count). An earlier write of the commit's own name (`writes`) is
///   never P.
/// - The members are the side parents (every parent but the first) of each
///   merge from the commit down to P, P excluded, the deepest merge's first,
///   carriers left out. So a cut of `[P, D, M6, M5]` and the chain
///   `merge(merge(merge(P, D), M6), M5)` read alike.
/// - A commit with one parent strictly between the commit and P refuses the
///   commit ([`UnexplainedCommit`]).
/// - With no P the commit is the series' first cut, and it reads as a
///   release outside a stacked series.
///
/// `writes` are the earlier writes of the commit's own name, and `trunks`
/// every trunk position ([`trunk_positions`]).
pub fn read_release(
    repo: &Repo,
    entry: &RepoEntry,
    (entries, tips, trunks): (&[Entry], &BookmarkTips, &[CommitId]),
    (writes, commit): (&EarlierWrites, &CommitId),
) -> Result<Result<ReleaseReading, UnexplainedCommit>, jj::JjError> {
    let parents = repo.parent_commits(commit.as_str())?;
    if entry.stacked()
        && let Some((between, previous)) =
            line_to_previous(repo, entry, (entries, tips, trunks), (writes, &parents))?
    {
        return Ok(stacked_members(writes, (&parents, &between), previous));
    }
    let (carriers, members) = writes.partition(&parents);
    Ok(Ok(ReleaseReading {
        previous: None,
        members,
        carriers,
    }))
}

/// The reading of a stacked release with its own `parents`, whose
/// first-parent line holds `between`, nearest first, each with its parents,
/// above its `previous` release ([`read_release`]).
fn stacked_members(
    writes: &EarlierWrites,
    (parents, between): (&[CommitId], &[(CommitId, Vec<CommitId>)]),
    previous: PreviousRelease,
) -> Result<ReleaseReading, UnexplainedCommit> {
    if let Some((commit, _)) = between.iter().find(|(_, parents)| parents.len() == 1) {
        return Err(UnexplainedCommit {
            commit: commit.clone(),
            previous,
        });
    }
    let side: Vec<CommitId> = between
        .iter()
        .rev()
        .map(|(_, parents)| parents.as_slice())
        .chain(std::iter::once(parents))
        .flat_map(|parents| parents.iter().skip(1).cloned())
        .collect();
    let (carriers, members) = writes.partition(&side);
    Ok(ReleaseReading {
        previous: Some(previous),
        members,
        carriers,
    })
}

/// Commits on a release's first-parent line, nearest first, each with its
/// own parents.
type FirstParentLine = Vec<(CommitId, Vec<CommitId>)>;

/// The first-parent line of a commit whose parents are `parents` down to its
/// previous release P ([`read_release`]): the commits above P, nearest first,
/// each with its own parents, and P. `None` when the line ends with no P: it
/// ends before the first commit a trunk position reaches, or at the root.
///
/// The walk stops at the first commit a cut or edit event of the series names
/// ([`recorded_previous_names`]): the nearest such commit is P. On the way it
/// remembers the first commit a release ref of the series names
/// ([`ref_previous_names`]), which is P only when the line ends with no
/// commit an event names. An earlier write of the commit's own name
/// (`writes`) is never P.
fn line_to_previous(
    repo: &Repo,
    entry: &RepoEntry,
    (entries, tips, trunks): (&[Entry], &BookmarkTips, &[CommitId]),
    (writes, parents): (&EarlierWrites, &[CommitId]),
) -> Result<Option<(FirstParentLine, PreviousRelease)>, jj::JjError> {
    let scheme = entry.release_scheme();
    let recorded = recorded_previous_names(entries, &scheme);
    let named = ref_previous_names(tips, (&scheme, entry.publish_remote()));
    let mut line = Vec::new();
    let mut nearest_named: Option<(usize, PreviousRelease)> = None;
    let mut next = parents.first().cloned();
    while let Some(at) = next {
        if trunk_reaches(repo, trunks, &at)? {
            break;
        }
        if !writes.contains(&at) {
            if let Some(&name) = recorded.get(&at) {
                let previous = PreviousRelease {
                    name: name.to_owned(),
                    commit: at,
                };
                return Ok(Some((line, previous)));
            }
            if nearest_named.is_none()
                && let Some(&(_, name)) = named.get(&at)
            {
                let previous = PreviousRelease {
                    name: name.to_string(),
                    commit: at.clone(),
                };
                nearest_named = Some((line.len(), previous));
            }
        }
        let parents = repo.parent_commits(at.as_str())?;
        next = parents.first().cloned();
        line.push((at, parents));
    }
    Ok(nearest_named.map(|(above, previous)| {
        line.truncate(above);
        (line, previous)
    }))
}

/// The name of each commit a cut or edit event of the series `scheme` names:
/// its write, under the event's name, and the previous release its parent
/// set labels ([`previous_release_label`]), under the label's name. Where two
/// events name one commit, the newer one's name is taken, and where one
/// event names it twice, its write's.
///
/// The nearest of these on a release's line is its previous release, not the
/// newest event's: an event of a release on another line, or a late `adopt`
/// of an older release, can name a commit further down the line, and the
/// release directly below the commit is its previous release.
///
/// The reap after a stacked cut forgets every ref of the previous release.
/// So for a previous release that no event records as a write (one cut by
/// hand), the label a later event gives it is the only record of it left.
fn recorded_previous_names<'a>(
    entries: &'a [Entry],
    scheme: &ReleaseScheme,
) -> BTreeMap<CommitId, &'a str> {
    let mut names = BTreeMap::new();
    // Events are read newest first, so the first name a commit takes stays.
    for event in entries.iter().rev() {
        let Some(subject) = event
            .subject
            .as_deref()
            .filter(|subject| scheme.owns(subject))
        else {
            continue;
        };
        let Some((_, write)) = recorded_write_events(std::slice::from_ref(event), subject).next()
        else {
            continue;
        };
        let labelled = event.parents.iter().filter_map(|parent| {
            let name = parent
                .branches
                .iter()
                .find_map(|branch| labelled_previous(branch))?;
            Some((CommitId::new(parent.commit.as_str()), name))
        });
        for (commit, name) in std::iter::once((write.commit, subject)).chain(labelled) {
            names.entry(commit).or_insert(name);
        }
    }
    names
}

/// The newest name a release ref of the series `scheme` gives each commit,
/// locally, on `publish_remote` or on origin ([`Role::Origin`]), with its
/// key ([`ReleaseScheme::cut_key`], so tags do not count). Of two refs whose
/// names are one name, the first is kept.
fn ref_previous_names<'a>(
    tips: &'a BookmarkTips,
    (scheme, publish_remote): (&ReleaseScheme, &str),
) -> BTreeMap<&'a CommitId, (crate::ids::CutKey<'a>, &'a BranchName)> {
    let origin = Role::Origin.to_string();
    let mut names: BTreeMap<&CommitId, (crate::ids::CutKey<'_>, &BranchName)> = BTreeMap::new();
    for (reference, commit) in tips {
        let ours = match reference {
            BookmarkRef::Local(_) => true,
            BookmarkRef::Remote { remote, .. } => {
                remote.as_str() == publish_remote || remote.as_str() == origin
            }
        };
        let Some(key) = scheme.cut_key(reference.branch().as_str()).filter(|_| ours) else {
            continue;
        };
        if names.get(commit).is_none_or(|(newest, _)| key > *newest) {
            names.insert(commit, (key, reference.branch()));
        }
    }
    names
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
            pr: None,
            parents: vec![RecordedParent {
                commit: "member0000000".to_owned(),
                branches: vec!["feat/member".to_owned()],
            }],
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
    use super::{CommitId, RecordedWrite, RecordedWriteKind, carrier_tests::event, unchecked_mark};
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
    }
}

#[cfg(test)]
mod reading_tests {
    use std::path::{Path, PathBuf};

    use super::{
        CommitId, EarlierWrites, Entry, PreviousRelease, RecordedParent, RecordedWrite,
        RecordedWriteKind, ReleaseReading, UnexplainedCommit,
        carrier_tests::{at, event},
        is_previous_release_label, last_recorded_cut, previous_release_label, read_release,
        recorded_members,
    };
    use crate::config::test_support::run_jj;
    use crate::config::{BaseRemote, RepoEntry, SeriesEntry};
    use crate::jj::Repo;

    /// The first cut of series `old`: the previous release of each release
    /// these tests read.
    const PREVIOUS: &str = "release/old/2026-08-04";

    /// The release after [`PREVIOUS`].
    const NEXT: &str = "release/old/2026-08-05";

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
    /// and `m5`), and two next releases made from them by hand: the merge
    /// `flat` of the cut and the three, and the chain `chain`, which is
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
        commit(&path, &[PREVIOUS, "del", "m6", "m5"], "flat");
        commit(&path, &[PREVIOUS, "del"], "inner1");
        commit(&path, &["inner1", "m6"], "inner2");
        commit(&path, &["inner2", "m5"], "chain");
        run_jj(&path, &["new", "-r", "root()"]);
        path
    }

    /// Series `old` of an entry, on the base `old`, stacked or not.
    fn series(stacked: bool) -> RepoEntry {
        let mut entry = RepoEntry::new(
            "https://forge.invalid/upstream/repo.git",
            "https://forge.invalid/fork/repo.git",
        );
        entry.series.insert(
            "old".to_owned(),
            SeriesEntry {
                base: "old".to_owned(),
                base_remote: BaseRemote::Upstream,
                stacked,
            },
        );
        entry.for_series("old").expect("select series old")
    }

    fn commits(path: &Path, revisions: &[&str]) -> Vec<CommitId> {
        revisions
            .iter()
            .map(|revision| at(path, revision))
            .collect()
    }

    /// `revision` as `entry` reads it, with `entries` as the ledger and the
    /// base `old` as the one trunk position.
    fn read(
        path: &Path,
        entry: &RepoEntry,
        entries: &[Entry],
        revision: &str,
    ) -> Result<ReleaseReading, UnexplainedCommit> {
        let repo = Repo::open(path).expect("open test repository");
        let tips = repo.bookmark_tips().expect("read bookmarks");
        let trunks = [at(path, "old")];
        let writes =
            EarlierWrites::read(&repo, revision, entries, None).expect("read earlier writes");
        read_release(
            &repo,
            entry,
            (entries, &tips, &trunks),
            (&writes, &at(path, revision)),
        )
        .expect("read the release")
    }

    /// [`PREVIOUS`] as the previous release.
    fn previous(path: &Path) -> PreviousRelease {
        PreviousRelease {
            name: PREVIOUS.to_owned(),
            commit: at(path, PREVIOUS),
        }
    }

    /// A cut event of `subject` whose write is `commit`.
    fn cut(subject: &str, commit: &CommitId) -> Entry {
        event(
            subject,
            format!("cut {subject} as {} with 1 parent(s): x", commit.short()),
            commit,
        )
    }

    #[test]
    fn a_flat_stacked_release_reads_as_its_previous_release_and_its_other_parents() {
        // Given: a release of a stacked series that is one merge of the
        // series' first cut and three branches on it, which no ledger event
        // records.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());

        // When: it is read.
        let reading = read(&path, &series(true), &[], "flat");

        // Then: its previous release is its first parent, which a release
        // ref of the series names, and its members are the other parents.
        assert_eq!(
            reading,
            Ok(ReleaseReading {
                previous: Some(previous(&path)),
                members: commits(&path, &["del", "m6", "m5"]),
                carriers: Vec::new(),
            })
        );
        // And: its composition is the previous release, then the members.
        assert_eq!(
            reading.as_ref().map(ReleaseReading::composition),
            Ok(commits(&path, &[PREVIOUS, "del", "m6", "m5"]))
        );
    }

    #[test]
    fn a_chain_of_merges_reads_as_the_flat_merge_of_the_same_members() {
        // Given: the same release made as a chain of merges, each adding one
        // member to the one before.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());

        // When: it is read.
        let reading = read(&path, &series(true), &[], "chain");

        // Then: it has the previous release and the members of the flat
        // merge, in the same order.
        assert_eq!(
            reading,
            Ok(ReleaseReading {
                previous: Some(previous(&path)),
                members: commits(&path, &["del", "m6", "m5"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn a_commit_with_one_parent_between_a_release_and_its_previous_release_refuses_it() {
        // Given: a commit with one parent on the previous release, and a
        // release that merges a member onto it.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());
        commit(&path, &[PREVIOUS], "bare");
        commit(&path, &["bare", "m6"], "refused");
        run_jj(&path, &["new", "-r", "root()"]);

        // When: the release is read.
        let reading = read(&path, &series(true), &[], "refused");

        // Then: the reading refuses it and names that commit.
        let bare = at(&path, "bare");
        assert_eq!(
            reading,
            Err(UnexplainedCommit {
                commit: bare.clone(),
                previous: previous(&path),
            })
        );
        let refusal = reading.err().as_ref().map(ToString::to_string);
        assert!(
            refusal
                .as_deref()
                .is_some_and(|refusal| refusal.contains(bare.short())
                    && refusal.contains(PREVIOUS)),
            "{refusal:?}"
        );
    }

    #[test]
    fn a_first_cut_has_no_previous_release_and_reads_its_parents() {
        // Given: the series' first cut, whose first-parent line reaches the
        // base without passing a release of the series.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());

        // When: it is read in the stacked series.
        let reading = read(&path, &series(true), &[], PREVIOUS);

        // Then: it has no previous release, and its parents are its members.
        assert_eq!(
            reading,
            Ok(ReleaseReading {
                previous: None,
                members: commits(&path, &["fix/a", "fix/b"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn the_newest_recorded_write_on_the_line_comes_before_a_nearer_release_ref() {
        // Given: a release ref of the series on the chain's inner merge of
        // `del`, nearer the chain than the first cut.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());
        run_jj(&path, &["bookmark", "create", NEXT, "-r", "inner1"]);

        // When: the chain is read with no ledger.
        let by_ref = read(&path, &series(true), &[], "chain");

        // Then: the nearest ref names its previous release, and only the
        // merges above that release bring members.
        assert_eq!(
            by_ref,
            Ok(ReleaseReading {
                previous: Some(PreviousRelease {
                    name: NEXT.to_owned(),
                    commit: at(&path, "inner1"),
                }),
                members: commits(&path, &["m6", "m5"]),
                carriers: Vec::new(),
            })
        );

        // Given: a ledger that records the first cut, then a newer cut of
        // the series whose write is off the chain's first-parent line, and
        // a newer cut of the default series on it.
        let entries = [
            cut(PREVIOUS, &at(&path, PREVIOUS)),
            cut("release/old/2026-08-06", &at(&path, "m6")),
            cut("release/2026-08-07", &at(&path, "inner1")),
        ];

        // When: the chain is read with that ledger.
        let by_record = read(&path, &series(true), &entries, "chain");

        // Then: the recorded first cut is its previous release, not the
        // nearer ref.
        assert_eq!(
            by_record,
            Ok(ReleaseReading {
                previous: Some(previous(&path)),
                members: commits(&path, &["del", "m6", "m5"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn a_labelled_previous_release_comes_before_a_nearer_release_ref() {
        // Given: the first cut with no ref left, a cut event of the next
        // release whose write is off the chain's first-parent line and which
        // labels the first cut as its previous release, and a release ref of
        // the series on the chain's inner merge of `del`, nearer the chain
        // than the first cut.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());
        let first_cut = at(&path, PREVIOUS);
        run_jj(&path, &["bookmark", "forget", PREVIOUS]);
        run_jj(&path, &["bookmark", "create", NEXT, "-r", "inner1"]);
        let start = at(&path, "flat");
        let mut labelling = event(
            NEXT,
            format!("cut {NEXT} as {} with 1 parent(s): x", start.short()),
            &start,
        );
        labelling.evidence = vec![start.as_str().to_owned(), first_cut.as_str().to_owned()];
        labelling.parents = vec![RecordedParent {
            commit: first_cut.as_str().to_owned(),
            branches: vec![previous_release_label(PREVIOUS)],
        }];

        // When: the chain is read.
        let reading = read(&path, &series(true), &[labelling], "chain");

        // Then: the labelled first cut is its previous release, not the
        // commit the nearer ref names, and every merge above it brings a
        // member.
        assert_eq!(
            reading,
            Ok(ReleaseReading {
                previous: Some(PreviousRelease {
                    name: PREVIOUS.to_owned(),
                    commit: first_cut,
                }),
                members: commits(&path, &["del", "m6", "m5"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn a_labelled_previous_release_is_read_when_no_write_or_ref_names_it() {
        // Given: the first cut with no ref left, as the reap after a stacked
        // cut leaves it, and a cut event of the next release that labels it
        // as the previous release. The event's write is off the chain's
        // first-parent line, and no event records the first cut as a write.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());
        let first_cut = at(&path, PREVIOUS);
        run_jj(&path, &["bookmark", "forget", PREVIOUS]);
        let labelling = |write: &CommitId, text: String| {
            let mut recorded = event(NEXT, text, write);
            recorded.evidence = vec![write.as_str().to_owned(), first_cut.as_str().to_owned()];
            recorded.parents = vec![RecordedParent {
                commit: first_cut.as_str().to_owned(),
                branches: vec![previous_release_label(PREVIOUS)],
            }];
            recorded
        };
        let start = at(&path, "flat");
        let mut entries = vec![labelling(
            &start,
            format!("cut {NEXT} as {} with 1 parent(s): x", start.short()),
        )];

        // When: the chain is read.
        let by_label = read(&path, &series(true), &entries, "chain");

        // Then: the labelled commit is its previous release, under the
        // label's name, and the merges above it bring the members.
        assert_eq!(
            by_label,
            Ok(ReleaseReading {
                previous: Some(PreviousRelease {
                    name: PREVIOUS.to_owned(),
                    commit: first_cut.clone(),
                }),
                members: commits(&path, &["del", "m6", "m5"]),
                carriers: Vec::new(),
            })
        );

        // Given: a newer edit event of the next release, whose write is the
        // chain's inner merge of `del` and which labels the first cut too.
        let inner = at(&path, "inner1");
        entries.push(labelling(
            &inner,
            format!(
                "edited {NEXT} as {} with 2 parent(s): included del",
                inner.short()
            ),
        ));

        // When: the chain is read again.
        let by_write = read(&path, &series(true), &entries, "chain");

        // Then: the write, nearer the chain than the label, is its previous
        // release, under the event's name.
        assert_eq!(
            by_write,
            Ok(ReleaseReading {
                previous: Some(PreviousRelease {
                    name: NEXT.to_owned(),
                    commit: inner,
                }),
                members: commits(&path, &["m6", "m5"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn the_nearest_recorded_commit_is_the_previous_release_whatever_the_event_order() {
        // Given: an older event that records the chain's inner merge of `del`
        // as a write, and a newer event, whose write is off the chain's line,
        // that labels the first cut further down the line: a release of
        // another line stacked on the first cut, or a late adopt.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());
        let first_cut = at(&path, PREVIOUS);
        run_jj(&path, &["bookmark", "forget", PREVIOUS]);
        let inner = at(&path, "inner1");
        let start = at(&path, "flat");
        let older = event(
            NEXT,
            format!(
                "edited {NEXT} as {} with 2 parent(s): included del",
                inner.short()
            ),
            &inner,
        );
        let mut newer = event(
            NEXT,
            format!("cut {NEXT} as {} with 1 parent(s): x", start.short()),
            &start,
        );
        newer.evidence = vec![start.as_str().to_owned(), first_cut.as_str().to_owned()];
        newer.parents = vec![RecordedParent {
            commit: first_cut.as_str().to_owned(),
            branches: vec![previous_release_label(PREVIOUS)],
        }];

        // When: the chain is read.
        let reading = read(&path, &series(true), &[older, newer], "chain");

        // Then: the nearer commit is its previous release, though an older
        // event names it, and the merges above it bring the members.
        assert_eq!(
            reading,
            Ok(ReleaseReading {
                previous: Some(PreviousRelease {
                    name: NEXT.to_owned(),
                    commit: inner,
                }),
                members: commits(&path, &["m6", "m5"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn outside_a_stacked_series_a_release_reads_its_own_parents() {
        // Given: the chain, in a series without the stacked key.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());

        // When: it is read.
        let reading = read(&path, &series(false), &[], "chain");

        // Then: it has no previous release, and its own parents are its
        // members.
        assert_eq!(
            reading,
            Ok(ReleaseReading {
                previous: None,
                members: commits(&path, &["inner2", "m5"]),
                carriers: Vec::new(),
            })
        );
    }

    #[test]
    fn the_previous_release_label_names_the_release_and_no_branch_reads_as_one() {
        assert_eq!(
            previous_release_label(PREVIOUS),
            "release/old/2026-08-04 previous release (not a member)"
        );
        assert!(is_previous_release_label(&previous_release_label(PREVIOUS)));
        assert!(!is_previous_release_label("del"));
        assert!(!is_previous_release_label(
            " previous release (not a member)"
        ));
    }

    /// A recorded parent set that labels `previous` as the previous release
    /// and records `member` under the branch `del`.
    fn stacked_parents(previous: &CommitId, member: &CommitId) -> Vec<RecordedParent> {
        vec![
            RecordedParent {
                commit: previous.as_str().to_owned(),
                branches: vec![previous_release_label(PREVIOUS)],
            },
            RecordedParent {
                commit: member.as_str().to_owned(),
                branches: vec!["del".to_owned()],
            },
        ]
    }

    #[test]
    fn a_recorded_cut_leaves_out_the_previous_release_its_parents_label() {
        // Given: a cut event whose evidence names the previous release and
        // one member, the previous release labelled as one, and the start
        // of a stacked cut, whose evidence names the previous release alone.
        let (release, previous, member) = (
            CommitId::new("c".repeat(40)),
            CommitId::new("e".repeat(40)),
            CommitId::new("d".repeat(40)),
        );
        let mut recorded = cut(NEXT, &release);
        recorded.evidence = [&release, &previous, &member]
            .iter()
            .map(|commit| commit.as_str().to_owned())
            .collect();
        recorded.parents = stacked_parents(&previous, &member);
        let mut start = recorded.clone();
        start.evidence.truncate(2);
        start.parents.truncate(1);

        // When: each is read as a recorded cut.
        let members =
            last_recorded_cut(std::slice::from_ref(&recorded), None).map(|read| read.members);
        let started =
            last_recorded_cut(std::slice::from_ref(&start), None).map(|read| read.members);

        // Then: the cut's one member is the member, and the start is still
        // a recorded cut, with no member.
        assert_eq!(members, Some(vec![member]));
        assert_eq!(started, Some(Vec::new()));
    }

    #[test]
    fn earlier_writes_and_recorded_members_leave_out_a_labelled_previous_release() {
        // Given: an edit event of the next release whose parent set labels
        // its previous release as one and records one member.
        let _environment = crate::config::test_support::environment_lock();
        let directory = tempfile::tempdir().expect("create test directory");
        let path = repository(directory.path());
        let (release, previous, member) =
            (at(&path, "flat"), at(&path, PREVIOUS), at(&path, "del"));
        let mut edit = event(
            NEXT,
            format!(
                "edited {NEXT} as {} with 2 parent(s): included del",
                release.short()
            ),
            &release,
        );
        edit.parents = stacked_parents(&previous, &member);
        let opened = Repo::open(&path).expect("open test repository");

        // When: the next release's parents are split into carriers and
        // members, and the event's members are read.
        let writes = EarlierWrites::read(&opened, NEXT, std::slice::from_ref(&edit), None)
            .expect("read earlier writes");
        let split = writes.partition(&[previous, member.clone()]);
        let write = RecordedWrite {
            kind: RecordedWriteKind::Edit,
            commit: release,
        };
        let recorded =
            recorded_members(&opened, (NEXT, &edit, &write), &writes).expect("read members");

        // Then: the previous release is neither a carrier nor a member.
        assert_eq!(split, (Vec::new(), vec![member.clone()]));
        assert_eq!(
            recorded,
            vec![RecordedParent {
                commit: member.as_str().to_owned(),
                branches: vec!["del".to_owned()],
            }]
        );
    }
}
