//! Release-domain rules shared by command verbs and reports.
//!
//! This module owns facts about release names, recorded cuts, and consumer pins.
//! Commands gather their I/O and render their answers around these rules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, Finding, FindingKind, Subject};
use crate::ids::{
    BookmarkRef, CommitId, RELEASE_PREFIX, ReleaseScheme, is_our_release, is_release_name,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, Kind};
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
    let last = entry.remote(Role::Origin).rsplit('/').next()?;
    let trimmed = last.trim_end_matches(".git");
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
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
/// trunk to measure past, and which commits our release refs name.
#[derive(Clone, Copy)]
pub struct StackedHistoryContext<'a> {
    pub repo: &'a Repo,
    pub trunk: &'a CommitId,
    pub releases: &'a BTreeMap<CommitId, Vec<BookmarkRef>>,
}

impl std::fmt::Debug for StackedHistoryContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StackedHistoryContext")
            .field("trunk", self.trunk)
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
/// pull request opened from it asks the maintainer to review the whole fork.
/// Observed on a real fork: a three-parent cut read "flat" while one parent
/// contained the previous cut's 26-parent merge; the same branch became an
/// upstream pull request of 61 commits and 140 files, and the maintainer asked
/// why it included "some extra things".
///
/// `None` when the history is linear. The finding names the merges and, for
/// each that one of our release refs names, which release it is.
pub fn stacked_history(
    context: StackedHistoryContext<'_>,
    branch: &str,
    tip: &CommitId,
) -> Result<Option<Finding>, jj::JjError> {
    let merges = context.repo.merges_between(context.trunk, tip)?;
    if merges.is_empty() {
        return Ok(None);
    }
    let named: Vec<String> = merges
        .iter()
        .filter_map(|merge| {
            context.releases.get(merge).map(|refs| {
                let names: Vec<String> = refs.iter().map(ToString::to_string).collect();
                format!("{} ({})", names.join(", "), short(merge))
            })
        })
        .collect();
    let releases_text = if named.is_empty() {
        String::new()
    } else {
        format!("; releases in that history: {}", named.join("; "))
    };
    Ok(Some(Finding::new(
        FindingKind::StackedHistory,
        Subject::Branch(crate::ids::BranchName::new(branch)),
        format!(
            "branch {branch}'s history past the upstream trunk carries {} merge commit(s), \
             so it carries everything those merges carried{releases_text}",
            merges.len()
        ),
    )))
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

    /// Whether the tip is a later state of the member whose released parent is
    /// `parent`.
    ///
    /// A parent the trunk already reaches has no successor here. Either it is a
    /// member that landed upstream by merge commit — every branch forked from
    /// the trunk since then descends from it, and none of them is that member —
    /// or it is the base a legacy cut carried as a parent. `knives release
    /// rebase` is what retires a landed member; a landed branch that kept
    /// growing is found through the record the cut or edit wrote under its
    /// name.
    pub fn succeeds(&self, parent: &CommitId) -> Result<bool, jj::JjError> {
        if parent == self.tip {
            return Ok(false);
        }
        for trunk in self.trunks {
            if self.repo.is_ancestor(parent, trunk)? {
                return Ok(false);
            }
        }
        if self.repo.is_ancestor(parent, self.tip)? {
            return Ok(true);
        }
        if self.own_changes.is_empty() {
            return Ok(false);
        }
        let parent_change = self.repo.change_id_of(parent.as_str())?;
        Ok(self
            .own_changes
            .iter()
            .any(|changes| changes.contains(&parent_change)))
    }
}

/// The local branches whose tip is a later state of the member released as
/// `parent`: where a stale parent's branch has gone.
///
/// "Carries no bookmark" is a poor report of a stale parent. The useful answer
/// is which branch that commit belonged to and where it moved, and after a
/// `jj rebase` only the change id still says so. Release names and the trunk
/// are not candidates; `branches` is the caller's list of maintained branches.
pub fn branches_succeeding(
    repo: &Repo,
    trunks: &[CommitId],
    parent: &CommitId,
    branches: &[(String, CommitId)],
) -> Result<Vec<(String, CommitId)>, jj::JjError> {
    let mut found = Vec::new();
    for (branch, tip) in branches {
        if MemberSuccession::of(repo, trunks, tip)?.succeeds(parent)? {
            found.push((branch.clone(), tip.clone()));
        }
    }
    Ok(found)
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
                    short(&first),
                    short(&different),
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
    /// Which branch each member was, as `(branch, first 12 hex of its commit)`,
    /// parsed from the event text. The one record of that pairing that survives
    /// a bookmark moving: after a rebase done outside jj — new commit ids, new
    /// change ids — nothing in the repository still ties the released parent to
    /// its branch name, and this does.
    pub named: Vec<(String, String)>,
}

/// `feat/alpha@0123456789ab, fix/beta@…`: the parent list a cut or edit event
/// carries, written here so it and [`named_members`] cannot drift apart.
pub fn members_event_text(members: &[(String, CommitId)]) -> String {
    members
        .iter()
        .map(|(source, commit)| format!("{source}@{}", short(commit)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `feat/alpha@0123456789ab, fix/beta@…` pairs from a cut or edit event's text.
///
/// A cut event lists them after `parent(s): `; an edit event after `parents: `.
/// The list runs to the end of the text or to the cut event's `; previous cut`
/// delta, whichever comes first.
fn named_members(text: &str) -> Vec<(String, String)> {
    let start = ["parent(s): ", "parents: "]
        .iter()
        .filter_map(|marker| text.rfind(marker).map(|at| at + marker.len()))
        .max();
    let Some(start) = start else {
        return Vec::new();
    };
    let after = text.get(start..).unwrap_or_default();
    let list = after.split("; previous cut").next().unwrap_or(after);
    list.split(", ")
        .filter_map(|pair| {
            let (name, prefix) = pair.trim().rsplit_once('@')?;
            (prefix.len() == 12 && prefix.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (name.to_owned(), prefix.to_owned()))
        })
        .collect()
}

/// The newest recorded parent set of `release`, as `(branch, commit prefix)`
/// pairs: from its last cut event or its last edit event, whichever is newer.
/// Empty when nothing was recorded.
pub fn recorded_parent_names(entries: &[Entry], release: &str) -> Vec<(String, String)> {
    let cut_prefix = format!("cut {release} as ");
    let edit_prefix = format!("edited {release}: ");
    entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(release))
        .filter(|entry| entry.text.starts_with(&cut_prefix) || entry.text.starts_with(&edit_prefix))
        .map(|entry| named_members(&entry.text))
        .find(|named| !named.is_empty())
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
            named: named_members(&entry.text),
        })
    })
}

fn short(value: &CommitId) -> String {
    value.as_str().chars().take(12).collect()
}
