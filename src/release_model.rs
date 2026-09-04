//! Release-domain rules shared by command verbs and reports.
//!
//! This module owns facts about release names, recorded cuts, and consumer pins.
//! Commands gather their I/O and render their answers around these rules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, Finding, FindingKind, Subject};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, RELEASE_PREFIX, ReleaseScheme, RemoteName, is_our_release,
    is_release_name,
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
    crate::bind::repository_name(entry.remote(Role::Origin)).map(str::to_owned)
}

/// The release the next cut carries: the local composition in hand, preferred
/// over the publish remote so unpushed release edits remain part of the cut.
pub fn previous_release_for_cut(
    entry: &RepoEntry,
    tips: &BookmarkTips,
) -> Option<(String, CommitId)> {
    let scheme = entry.release_scheme();
    newest_release(tips, &scheme, entry.publish_remote())
        .map(|(reference, commit)| (reference.to_string(), commit))
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
    let tips = repo.bookmark_tips()?;
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
    Ok(positions)
}

/// The newest release under the configured scheme and publish remote.
pub fn newest_release(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> Option<(BookmarkRef, CommitId)> {
    match scheme {
        ReleaseScheme::Dated => tips
            .iter()
            .filter(|(reference, _)| is_our_release(reference, scheme, publish_remote))
            .max_by_key(|(reference, _)| {
                (
                    release_order(reference.branch().as_str()),
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

/// Order a dated release name so numeric suffixes compare numerically.
pub fn release_order(name: &str) -> (String, u32) {
    let bare = name.strip_prefix(RELEASE_PREFIX).unwrap_or(name);
    match bare.split_once('.') {
        Some((date, suffix)) => (date.to_owned(), suffix.parse().unwrap_or(0)),
        None => (bare.to_owned(), 0),
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
/// trunk positions to measure past, and which commits our release refs name.
#[derive(Clone, Copy)]
pub struct StackedHistoryContext<'a> {
    pub repo: &'a Repo,
    /// Every known trunk position ([`trunk_positions`]): a merge counts only
    /// when it joins lines none of them reaches.
    pub trunks: &'a [CommitId],
    pub releases: &'a BTreeMap<CommitId, Vec<BookmarkRef>>,
}

impl std::fmt::Debug for StackedHistoryContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StackedHistoryContext")
            .field("trunks", &self.trunks)
            .field("releases", &self.releases.len())
            .finish_non_exhaustive()
    }
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
/// `None` when the history is linear. The finding names the merges and, for
/// each that one of our release refs names, which release it is. A merge no
/// release ref names may be upstream's own, visible only because every local
/// view of the trunk is behind the branch's base; the finding says so, since
/// the local repository cannot exclude it without fetching.
pub fn stacked_history(
    context: StackedHistoryContext<'_>,
    branch: &str,
    tip: &CommitId,
) -> Result<Option<Finding>, jj::JjError> {
    let merges = context.repo.merges_between(context.trunks, tip)?;
    if merges.is_empty() {
        return Ok(None);
    }
    let named: Vec<String> = merges
        .iter()
        .filter_map(|merge| {
            context.releases.get(merge).map(|refs| {
                let names: Vec<String> = refs.iter().map(ToString::to_string).collect();
                format!("{} ({})", names.join(", "), merge.short())
            })
        })
        .collect();
    let releases_text = if named.is_empty() {
        "; if the branch forked from a newer upstream than the local trunk views, those \
         are upstream's own merges and `knives sync` fetches them"
            .to_owned()
    } else {
        format!("; releases in that history: {}", named.join("; "))
    };
    Ok(Some(Finding::new(
        FindingKind::StackedHistory,
        Subject::Branch(BranchName::new(branch)),
        format!(
            "branch {branch}'s history past the trunk carries {} merge commit(s), \
             so it carries everything those merges carried{releases_text}",
            merges.len()
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

/// The composition a previous cut's ledger event recorded.
#[derive(Debug, PartialEq, Eq)]
pub struct RecordedCut {
    pub name: String,
    /// The commit created by this cut, stored as the first evidence item.
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

/// The newest structural cut event, optionally scoped to a release name.
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
            name: entry_subject.to_owned(),
            commit: CommitId::new(commit.as_str()),
            members: members
                .iter()
                .map(|sha| CommitId::new(sha.as_str()))
                .collect(),
        })
    })
}
