//! Spec R11: an edit of a release (`include`, `drop` or `advance`), built
//! in a scratch transaction by R11's rule ([`edit_candidate`],
//! [`ruled_tree`]), and R11's check of that candidate before anything is
//! written ([`EditCandidate::lost_lines`]).
//!
//! It is reached only through [`edit_candidate`] and
//! [`super::duplicated_release`], and it stays in [`super`], the one module
//! that opens a repository.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use jj_lib::backend::{BackendError, MergedTreeValue};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::conflicts::{
    ConflictMarkerStyle, ConflictMaterializeOptions, MaterializedFileConflictValue,
    MaterializedTreeValue, choose_materialized_conflict_marker_len, extract_as_single_hunk,
    materialize_merge_result_to_bytes, materialize_tree_value,
};
use jj_lib::diff::{ContentDiff, DiffHunkKind};
use jj_lib::files::{MergeResult, merge_hunks};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::transaction::Transaction;
use jj_lib::tree_merge::MergeOptions;

use super::{
    BuiltRelease, JjError, PlainMergedFile, ReleaseWrite, ReleaseWritten, Repo, UnionedFile,
    block_on, collect_stream, commit_id, duplicated_from, finished_release, own_value,
    regular_file_content, regular_files, resolved_parents, store_error, stored_value,
    validate_parents,
};
use crate::changelog_union;
use crate::ids::CommitId;
use crate::own_lines::{self, Lost, SkippedRange};

/// A member's own change: each path where the trees of its base by spec R6
/// and its tip differ, with the two values there.
type ChangeValues = BTreeMap<RepoPathBuf, (MergedTreeValue, MergedTreeValue)>;

impl Repo {
    /// Each path where the trees of `base` and `tip` differ, read with no
    /// copy records, with the values there: a member's own change, as
    /// [`Self::member_lines`] reads it.
    fn change_values(&self, (base, tip): (&CommitId, &CommitId)) -> Result<ChangeValues, JjError> {
        let base_tree = self.commit(base.as_str())?.tree();
        let tip_tree = self.commit(tip.as_str())?.tree();
        let mut change = BTreeMap::new();
        for entry in collect_stream(base_tree.diff_stream(&tip_tree, &EverythingMatcher)) {
            let values = entry.values.map_err(|error| store_error(&error))?;
            change.insert(entry.path, (values.before, values.after));
        }
        Ok(change)
    }

    /// Each line of the change `tip` makes from `base` at `path`
    /// ([`own_lines::change_lines`]).
    fn change_lines_at(
        &self,
        path: &RepoPath,
        (base, tip): (&CommitId, &CommitId),
    ) -> Result<Vec<Lost>, JjError> {
        let store = self.repo.store();
        let value_at = |commit: &CommitId| -> Result<own_lines::Value, JjError> {
            let tree = self.commit(commit.as_str())?.tree();
            let value = block_on(tree.path_value(path)).map_err(|error| store_error(&error))?;
            own_value(store, path, &value)
        };
        Ok(own_lines::change_lines(
            path.as_internal_file_string(),
            &value_at(base)?,
            &value_at(tip)?,
        ))
    }
}

/// An edit of a release (`include`, `drop` or `advance`) built in a scratch
/// transaction that is never committed, so that spec R11's check reads it
/// before anything is written ([`EditCandidate::lost_lines`]).
///
/// The candidate is [`super::write_release`]'s commit written by spec R11's
/// rule ([`ruled_tree`]). A `CHANGELOG.md` or `RELEASE_NOTES.md` whose
/// conflict the release-log union joins keeps what the duplicate writes
/// there. So does a file whose every conflict hunk the release still leaves
/// open, with the sides it had. Any other file whose conflict the release
/// resolved keeps that resolution only where the plain merge of the new
/// parents leaves the same conflict there. Elsewhere it is written as that
/// plain merge, and the edit names the file where that changed it
/// ([`EditCandidate::publish`]). Then the release-log union runs, as on
/// every write.
/// [`EditCandidate::publish`] rebuilds it in a fresh transaction, refuses if
/// the rebuilt tree differs from the checked one, as
/// [`super::Candidate::publish`] does, and then makes the write as
/// [`super::write_release`] makes it, in one operation. A candidate that is
/// dropped writes nothing: the operation log and every bookmark stay as they
/// were.
pub struct EditCandidate<'w> {
    repo: Repo,
    tx: Transaction,
    write: &'w ReleaseWrite<'w>,
    /// The write's source, the release in hand, which [`edit_candidate`]
    /// checked the write names.
    source: &'w CommitId,
    /// What the scratch transaction holds.
    built: BuiltEdit,
}

impl std::fmt::Debug for EditCandidate<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditCandidate")
            .field("write", self.write)
            .field("commit", self.built.written.id())
            .finish_non_exhaustive()
    }
}

/// Build an edit's `write` in a scratch transaction, for spec R11's check
/// ([`EditCandidate`]).
///
/// An edit duplicates the release in hand, so the write must name its
/// source: one that names none is refused before anything is built, as an
/// empty or repeated parent set is.
pub fn edit_candidate<'w>(
    repo: &Path,
    write: &'w ReleaseWrite<'w>,
) -> Result<EditCandidate<'w>, JjError> {
    validate_parents(write.source, write.parents)?;
    let Some(source) = write.source else {
        return Err(JjError::Revision {
            revision: "a fresh merge".to_owned(),
            detail: "an edit duplicates the release in hand, and this write names none".to_owned(),
        });
    };
    let repo = Repo::open(repo)?;
    let mut tx = repo.repo.start_transaction();
    let built = built_edit(&repo, &mut tx, write, source)?;
    Ok(EditCandidate {
        repo,
        tx,
        write,
        source,
        built,
    })
}

/// A remaining member of an edited release, as spec R11's check reads it
/// ([`EditCandidate::lost_lines`]).
#[derive(Debug, Clone)]
pub struct EditMember<'a> {
    /// How the report names the member.
    pub label: &'a str,
    /// Its own change: from its base by spec R6 to its tip, the base read
    /// from the release in hand's other parents where the trunk holds the
    /// member on its first-parent line.
    pub change: (CommitId, CommitId),
    /// What of that change this edit brings into the release.
    pub brought: Brought,
}

/// What of a member's change an edit brings into the release (spec R11's
/// second condition).
#[derive(Debug, Clone)]
pub enum Brought {
    /// Nothing: the release in hand holds the member at this tip.
    Nothing,
    /// Every line: an included member.
    Whole,
    /// The lines that its change has more often than the change of its old
    /// tip, from that tip's base (the first id) to that tip (the second),
    /// compared by path, kind and text: an advanced member.
    Since(CommitId, CommitId),
}

/// The lines of one member's change that an edit loses at one path, as
/// spec R11's check counts them ([`EditCandidate::lost_lines`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditLoss {
    /// The repository-internal path, `/`-separated.
    pub path: String,
    /// The member, as the report names it ([`EditMember::label`]).
    pub label: String,
    /// Each line counted.
    pub lost: Vec<Lost>,
    /// Each range at the path that the word step did not follow.
    pub word_step_skipped: Vec<SkippedRange>,
}

impl EditCandidate<'_> {
    /// Spec R11's check of the candidate: each line of a remaining member's
    /// change that the edit loses, by path, then by member in `members`'
    /// order.
    ///
    /// The paths read are each path where the candidate differs from the
    /// release in hand, and each path that the change of an included or
    /// advanced member touches. Each member whose own change touches such a
    /// path is read there in three versions: the candidate, the release in
    /// hand and the plain merge of the new parents, each with its conflicts
    /// written out in jj's snapshot style ([`placing`],
    /// [`own_lines::place`]). A line counts where all three conditions hold
    /// ([`counted_lines`]).
    pub fn lost_lines(&self, members: &[EditMember<'_>]) -> Result<Vec<EditLoss>, JjError> {
        let (release, candidate) = (self.built.source.tree(), self.built.written.tree());
        let mut paths = BTreeSet::new();
        for entry in collect_stream(release.diff_stream(&candidate, &EverythingMatcher)) {
            entry.values.map_err(|error| store_error(&error))?;
            paths.insert(entry.path);
        }
        let mut changes = Vec::with_capacity(members.len());
        for member in members {
            let (base, tip) = &member.change;
            let change = self.repo.change_values((base, tip))?;
            if !matches!(member.brought, Brought::Nothing) {
                paths.extend(change.keys().cloned());
            }
            changes.push(change);
        }
        let mut losses = Vec::new();
        for path in &paths {
            let mut versions = PathVersions::new(&release, &candidate);
            for (member, change) in members.iter().zip(&changes) {
                let Some(values) = change.get(path) else {
                    continue;
                };
                losses.extend(self.lost_at((path, values), member, &mut versions)?);
            }
        }
        Ok(losses)
    }

    /// The lines of `member`'s change at `path`, from its base's value to
    /// its tip's, that the edit loses ([`Self::lost_lines`]); `None`
    /// where it loses none. `versions` keeps what this path holds in each
    /// version once a member at the path read it.
    fn lost_at(
        &self,
        (path, (before, after)): (&RepoPath, &(MergedTreeValue, MergedTreeValue)),
        member: &EditMember<'_>,
        versions: &mut PathVersions<'_>,
    ) -> Result<Option<EditLoss>, JjError> {
        let store = self.repo.repo.store();
        let value_at = |tree: &MergedTree| {
            block_on(tree.path_value(path)).map_err(|error| store_error(&error))
        };
        let in_candidate = read_once(&mut versions.in_candidate, || {
            value_at(versions.candidate_tree)
        })?;
        if after.is_resolved() && in_candidate == after {
            return Ok(None);
        }
        let name = path.as_internal_file_string();
        let (fork, tip) = (
            own_value(store, path, before)?,
            own_value(store, path, after)?,
        );
        let read = |version: &own_lines::Version| own_lines::place(name, (&fork, &tip), version);
        let candidate = read_once(&mut versions.candidate, || {
            placing(store, path, in_candidate.clone())
        })?;
        let own_lines::Placed::Lines {
            lost,
            word_step_skipped,
        } = read(candidate)
        else {
            return Ok(None);
        };
        if lost.iter().all(|line| line.at_conflict) {
            return Ok(None);
        }
        let release = read(read_once(&mut versions.release, || {
            placing(store, path, value_at(versions.release_tree)?)
        })?);
        let plain = read(read_once(&mut versions.plain, || {
            placing(store, path, value_at(&self.built.plain)?)
        })?);
        let brought = match &member.brought {
            Brought::Nothing => BroughtLines::Nothing,
            Brought::Whole => BroughtLines::Whole,
            Brought::Since(old_base, old_tip) => BroughtLines::Since {
                old: self.repo.change_lines_at(path, (old_base, old_tip))?,
                new: own_lines::change_lines(name, &fork, &tip),
            },
        };
        let counted = counted_lines(lost, (&release, &plain), &brought);
        if counted.is_empty() {
            return Ok(None);
        }
        Ok(Some(EditLoss {
            path: name.to_owned(),
            label: member.label.to_owned(),
            lost: counted,
            word_step_skipped,
        }))
    }

    /// Rebuild the checked edit in a fresh transaction and write it as
    /// [`super::write_release`] writes a release, as ONE operation, with the
    /// files spec R11's rule wrote as the plain merge of the new parents.
    /// Refuses where the rebuilt tree differs from the checked one, so the
    /// check holds for what is written, as [`super::Candidate::publish`]
    /// refuses; a rebuild of the same inputs never differs.
    pub fn publish(self) -> Result<(ReleaseWritten, Vec<PlainMergedFile>), JjError> {
        let Self {
            repo,
            tx,
            write,
            source,
            built: checked,
        } = self;
        // The checked scratch evaporates before anything real is written.
        drop(tx);
        let repo = repo.to_write()?;
        let mut tx = repo.repo.start_transaction();
        let BuiltEdit {
            written,
            source: release,
            unions,
            plain_merged,
            ..
        } = built_edit(&repo, &mut tx, write, source)?;
        if written.tree_ids() != checked.written.tree_ids() {
            return Err(JjError::Revision {
                revision: commit_id(checked.written.id()).as_str().to_owned(),
                detail: "the rebuilt edit's tree differs from the checked candidate".to_owned(),
            });
        }
        let built = BuiltRelease {
            written,
            source: Some(release),
            unions,
        };
        Ok((finished_release(&repo, tx, write, built)?, plain_merged))
    }
}

/// What [`built_edit`] wrote in a transaction.
struct BuiltEdit {
    written: jj_lib::commit::Commit,
    /// The release in hand: the write's source.
    source: jj_lib::commit::Commit,
    unions: Vec<UnionedFile>,
    /// The plain merge of the new parents, with no release-log union.
    plain: MergedTree,
    /// The files spec R11's rule wrote as that plain merge.
    plain_merged: Vec<PlainMergedFile>,
}

/// The release commit an edit's `write` describes, written into `tx` and
/// not yet named: the duplicate of `source`, the release in hand, onto the
/// write's parents, written by spec R11's rule ([`ruled_tree`]), with the
/// release-log union, as [`super::built_release`] writes any other
/// duplicate.
fn built_edit(
    repo: &Repo,
    tx: &mut Transaction,
    write: &ReleaseWrite<'_>,
    source: &CommitId,
) -> Result<BuiltEdit, JjError> {
    let (parent_commits, parent_ids) = resolved_parents(repo, write.parents)?;
    let source = repo.commit(source.as_str())?;
    let measured = write.measured.parents(&source);
    let plain = block_on(merge_commit_trees(tx.repo(), &parent_commits))
        .map_err(|error| store_error(&error))?;
    let (written, unions, plain_merged) = duplicated_from(
        tx,
        (&source, write.message),
        measured,
        (&parent_ids, Some(&plain)),
    )?;
    Ok(BuiltEdit {
        written,
        source,
        unions,
        plain,
        plain_merged,
    })
}

/// One path, as spec R11's check reads it in each version
/// ([`EditCandidate::lost_at`]): each value is read the first time a member
/// at the path needs it, and kept for the path's other members.
struct PathVersions<'t> {
    /// The trees of the release in hand and of the candidate.
    release_tree: &'t MergedTree,
    candidate_tree: &'t MergedTree,
    /// The candidate's value at the path.
    in_candidate: Option<MergedTreeValue>,
    /// The candidate, the release in hand and the plain merge of the new
    /// parents at the path, each as [`placing`] reads it.
    candidate: Option<own_lines::Version>,
    release: Option<own_lines::Version>,
    plain: Option<own_lines::Version>,
}

impl<'t> PathVersions<'t> {
    /// A path of which nothing is read yet.
    const fn new(release_tree: &'t MergedTree, candidate_tree: &'t MergedTree) -> Self {
        Self {
            release_tree,
            candidate_tree,
            in_candidate: None,
            candidate: None,
            release: None,
            plain: None,
        }
    }
}

/// What `slot` holds, read with `read` where it holds nothing yet.
fn read_once<T>(
    slot: &mut Option<T>,
    read: impl FnOnce() -> Result<T, JjError>,
) -> Result<&T, JjError> {
    let value = slot.take().map_or_else(read, Ok)?;
    Ok(slot.insert(value))
}

/// What `value`, one tree's value at `path`, holds as spec R11 places a
/// member's lines in it ([`own_lines::place`]).
///
/// A resolved value is what [`own_value`] reads, as R9 reads it: a file is
/// its content, an absent path or a directory holds no file, and a symlink
/// or a submodule is what it is, which gives no place. A conflict between
/// files is written out in jj's snapshot style
/// ([`ConflictMarkerStyle::Snapshot`], [`conflict_text`]), with its conflict
/// regions. Any other conflict, and a value the store refuses to read
/// ([`BackendError::ReadAccessDenied`]), is [`own_lines::Value::Conflict`],
/// which gives no place.
fn placing(
    store: &jj_lib::store::Store,
    path: &RepoPath,
    value: MergedTreeValue,
) -> Result<own_lines::Version, JjError> {
    let conflict = || own_lines::Version {
        value: own_lines::Value::Conflict,
        regions: Vec::new(),
        conflicted: true,
    };
    if value.is_resolved() {
        return match stored_value(store, path, &value) {
            Ok(value) => Ok(own_lines::Version {
                value,
                regions: Vec::new(),
                conflicted: false,
            }),
            Err(BackendError::ReadAccessDenied { .. }) => Ok(conflict()),
            Err(error) => Err(store_error(&error)),
        };
    }
    Ok(match materialized(store, path, value)? {
        MaterializedTreeValue::FileConflict(file) => {
            let (content, marker_len) = conflict_text(store, &file, ConflictMarkerStyle::Snapshot);
            own_lines::Version {
                regions: own_lines::conflict_regions(&content, marker_len),
                value: own_lines::Value::File {
                    executable: file.executable.unwrap_or_default(),
                    content,
                },
                conflicted: true,
            }
        }
        // A value that is not resolved materializes as a conflict between
        // files, any other conflict, or a conflict the store refuses to read.
        _ => conflict(),
    })
}

/// `value`, one tree's value at `path`, as jj materializes it
/// ([`materialize_tree_value`]).
fn materialized(
    store: &jj_lib::store::Store,
    path: &RepoPath,
    value: MergedTreeValue,
) -> Result<MaterializedTreeValue, JjError> {
    block_on(materialize_tree_value(
        store,
        path,
        value,
        &ConflictLabels::unlabeled(),
    ))
    .map_err(|error| store_error(&error))
}

/// The text of `file`, a conflict between files, as jj writes it with its
/// conflict markers in `style`, and the length of those markers, which jj
/// chooses longer than any marker-like line of the terms
/// ([`choose_materialized_conflict_marker_len`]).
fn conflict_text(
    store: &jj_lib::store::Store,
    file: &MaterializedFileConflictValue,
    style: ConflictMarkerStyle,
) -> (Vec<u8>, usize) {
    let marker_len = choose_materialized_conflict_marker_len(&file.contents);
    let options = ConflictMaterializeOptions {
        marker_style: style,
        marker_len: Some(marker_len),
        merge: store.merge_options().clone(),
    };
    let content = Vec::from(materialize_merge_result_to_bytes(
        &file.contents,
        &file.labels,
        &options,
    ));
    (content, marker_len)
}

/// What of a member's change an edit brings in, read at one path
/// ([`Brought`]).
enum BroughtLines {
    Nothing,
    Whole,
    /// An advanced member's lines at the path, of its old tip's change and
    /// of its new tip's ([`own_lines::change_lines`]).
    Since {
        old: Vec<Lost>,
        new: Vec<Lost>,
    },
}

impl BroughtLines {
    /// Whether the edit brings `lost` in: every line of an included member,
    /// and, of an advanced one, a line whose path, kind and text its new
    /// change has more often than its old one.
    fn brings(&self, lost: &Lost) -> bool {
        match self {
            Self::Nothing => false,
            Self::Whole => true,
            Self::Since { old, new } => {
                let count = |lines: &[Lost]| {
                    lines
                        .iter()
                        .filter(|line| {
                            line.path == lost.path
                                && line_kind(line.kind) == line_kind(lost.kind)
                                && line.text == lost.text
                        })
                        .count()
                };
                count(new) > count(old)
            }
        }
    }
}

/// A lost entry's kind as spec R11 tells lines apart: a line in a range
/// that the word step did not follow is an added line.
const fn line_kind(kind: own_lines::LostKind) -> own_lines::LostKind {
    match kind {
        own_lines::LostKind::WordLimit => own_lines::LostKind::Added,
        other => other,
    }
}

/// Whether `one` and `other` are one line of a member's change: the same
/// path, kind ([`line_kind`]) and line. The readings are of one change at
/// one tip, so two lines of the same text stay apart.
fn same_line(one: &Lost, other: &Lost) -> bool {
    one.path == other.path && line_kind(one.kind) == line_kind(other.kind) && one.line == other.line
}

/// Whether `placed`, a member's change read in one version, holds `lost`
/// there or has it at a conflict.
fn held_or_at_conflict(placed: &own_lines::Placed, lost: &Lost) -> bool {
    match placed {
        own_lines::Placed::AtConflict => true,
        own_lines::Placed::Lines { lost: lines, .. } => !lines
            .iter()
            .any(|line| !line.at_conflict && same_line(&line.lost, lost)),
    }
}

/// Spec R11's count: each line of `candidate`, a member's change read in
/// the candidate, that the edit loses. A line counts where all three hold:
/// it is lost in the candidate at no conflict there; `release`, the release
/// in hand, holds it or has it at a conflict, or the edit brings it in
/// (`brought`); and `plain`, the plain merge of the new parents, holds it or
/// has it at a conflict. A line the release in hand already lacked is not
/// this edit's loss, and one the plain merge lacks with no conflict there
/// is the members' own change.
fn counted_lines(
    candidate: Vec<own_lines::PlacedLost>,
    (release, plain): (&own_lines::Placed, &own_lines::Placed),
    brought: &BroughtLines,
) -> Vec<Lost> {
    candidate
        .into_iter()
        .filter(|line| !line.at_conflict)
        .map(|line| line.lost)
        .filter(|lost| {
            (held_or_at_conflict(release, lost) || brought.brings(lost))
                && held_or_at_conflict(plain, lost)
        })
        .collect()
}

/// `duplicate`, the duplicate of `source` onto the new parents, written by
/// spec R11's rule for an edit, and each path the rule wrote as `new`, the
/// plain merge of the new parents, with whether a hand change was left out
/// there.
///
/// The rule reads the plain merge of `source`'s own parents, the release in
/// hand's members (its carriers left out, [`super::duplicated_from`]), and
/// acts on each path where that merge leaves a conflict
/// ([`RuleTrees::ruled`]): there the release's own change answers the
/// conflict. It keeps that resolution only where `new` leaves the same
/// conflict, and otherwise sets the value of `new` there, conflicted, clean
/// or absent. Every other path keeps the duplicate's value. Both merges are
/// jj's own, as `jj new` makes them, with no release-log union: the union
/// runs over the tree written here, as on every write
/// ([`super::duplicated_release`]).
pub(super) fn ruled_tree(
    repo: &dyn jj_lib::repo::Repo,
    (source, duplicate): (&jj_lib::commit::Commit, MergedTree),
    new: &MergedTree,
) -> Result<(MergedTree, Vec<(RepoPathBuf, bool)>), JjError> {
    let old_parents = block_on(source.parents()).map_err(|error| store_error(&error))?;
    let old =
        block_on(merge_commit_trees(repo, &old_parents)).map_err(|error| store_error(&error))?;
    if !old.has_conflict() {
        return Ok((duplicate, Vec::new()));
    }
    let trees = RuleTrees {
        release: source.tree(),
        new: new.clone(),
        duplicate,
    };
    let sides = trees.duplicate.tree_ids().num_sides();
    let mut builder = MergedTreeBuilder::new(trees.duplicate.clone());
    let mut written = Vec::new();
    for (path, value) in old.conflicts() {
        let value = value.map_err(|error| store_error(&error))?.simplify();
        if let Some((plain, hand_change_left_out)) = trees.ruled(&path, &value)? {
            builder.set_or_remove(path.clone(), padded(plain, sides));
            written.push((path, hand_change_left_out));
        }
    }
    if written.is_empty() {
        return Ok((trees.duplicate, written));
    }
    let tree = block_on(builder.write_tree()).map_err(|error| store_error(&error))?;
    Ok((tree, written))
}

/// The trees spec R11's rule reads at a path ([`ruled_tree`]).
struct RuleTrees {
    /// The release in hand's own tree.
    release: MergedTree,
    /// The plain merge of the new parents.
    new: MergedTree,
    /// The duplicate of the release onto the new parents.
    duplicate: MergedTree,
}

impl RuleTrees {
    /// What the rule writes at `path`, where the plain merge of the release
    /// in hand's own parents leaves the conflict `old` (simplified): `None`
    /// keeps the duplicate's value; otherwise the new parents' plain merge
    /// at the path, and whether the release had changed lines outside that
    /// conflict ([`changed_outside`]).
    ///
    /// A release log ([`changelog_union::eligible`]) whose conflict the union
    /// joins is left to the union ([`left_to_union`]). Every other path keeps
    /// the duplicate's value where [`keeps_duplicate`] says so, or where the
    /// new merge holds what the duplicate holds there: then nothing changes,
    /// and nothing is named.
    fn ruled(
        &self,
        path: &RepoPath,
        old: &MergedTreeValue,
    ) -> Result<Option<(MergedTreeValue, bool)>, JjError> {
        let store = self.duplicate.store();
        let options = store.merge_options();
        let value_at = |tree: &MergedTree| {
            block_on(tree.path_value(path)).map_err(|error| store_error(&error))
        };
        let release = value_at(&self.release)?;
        let terms = regular_terms(store, path, old)?;
        if changelog_union::eligible(path.as_internal_file_string()) {
            let earlier = regular_file_content(store, path, &release)?;
            if terms
                .as_ref()
                .is_some_and(|terms| left_to_union(terms, earlier.as_deref(), options))
            {
                return Ok(None);
            }
        }
        let merged = terms.as_ref().map(|terms| merge_hunks(terms, options));
        let answered = merged.as_ref().map_or_else(
            || ConflictHunks::Whole(old.clone()),
            |merged| ConflictHunks::Text(hunk_list(merged)),
        );
        let new = value_at(&self.new)?;
        if keeps_duplicate(
            &answered,
            conflict_at(store, path, &release)?.as_ref(),
            conflict_at(store, path, &new)?.as_ref(),
        ) || new.simplify() == value_at(&self.duplicate)?.simplify()
        {
            return Ok(None);
        }
        let hand_change_left_out = match (merged, materialized_file(store, path, release)?) {
            (Some(MergeResult::Conflict(hunks)), Some(file)) => changed_outside(&hunks, &file),
            _ => false,
        };
        Ok(Some((new, hand_change_left_out)))
    }
}

/// The terms of `value`, a conflict, as the contents of regular files, read
/// as [`super::union_release_logs`] reads a conflict ([`regular_files`]);
/// `None` where a term is not a regular file of the others' mode.
fn regular_terms(
    store: &jj_lib::store::Store,
    path: &RepoPath,
    value: &MergedTreeValue,
) -> Result<Option<Merge<impl AsRef<[u8]> + Clone + PartialEq + use<>>>, JjError> {
    let Some(files) = regular_files(value) else {
        return Ok(None);
    };
    block_on(extract_as_single_hunk(&files.ids, store, path))
        .map(Some)
        .map_err(|error| store_error(&error))
}

/// One unresolved hunk of a conflict between regular files, as spec R11
/// compares it: its bases and its sides, each sorted, so each reads as a
/// multiset that counts how often each text occurs.
#[derive(Debug, PartialEq, Eq)]
struct ConflictHunk {
    bases: Vec<Vec<u8>>,
    sides: Vec<Vec<u8>>,
}

/// The conflict a value leaves at a path, as spec R11 compares it.
#[derive(Debug)]
enum ConflictHunks {
    /// A conflict between regular files: its unresolved hunks in file order
    /// ([`hunk_list`]).
    Text(Vec<ConflictHunk>),
    /// Any other conflict, compared whole: its simplified terms, its bases
    /// and its sides each read as a multiset ([`same_terms`]).
    Whole(MergedTreeValue),
}

impl PartialEq for ConflictHunks {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(ours), Self::Text(theirs)) => ours == theirs,
            (Self::Whole(ours), Self::Whole(theirs)) => {
                same_terms(ours.removes(), theirs.removes())
                    && same_terms(ours.adds(), theirs.adds())
            }
            _ => false,
        }
    }
}

/// Whether `ours` and `theirs` hold the same terms, each as many times, in
/// any order.
fn same_terms<'a, T: PartialEq + 'a>(
    ours: impl Iterator<Item = &'a T>,
    theirs: impl Iterator<Item = &'a T>,
) -> bool {
    let (ours, theirs): (Vec<&T>, Vec<&T>) = (ours.collect(), theirs.collect());
    let count = |terms: &[&T], term: &T| terms.iter().filter(|other| **other == term).count();
    ours.len() == theirs.len()
        && ours
            .iter()
            .all(|&term| count(ours.as_slice(), term) == count(theirs.as_slice(), term))
}

/// The conflict `value` leaves at `path` as spec R11 compares it, or `None`
/// where it is resolved.
fn conflict_at(
    store: &jj_lib::store::Store,
    path: &RepoPath,
    value: &MergedTreeValue,
) -> Result<Option<ConflictHunks>, JjError> {
    let value = value.simplify();
    if value.is_resolved() {
        return Ok(None);
    }
    let Some(terms) = regular_terms(store, path, &value)? else {
        return Ok(Some(ConflictHunks::Whole(value)));
    };
    Ok(Some(ConflictHunks::Text(hunk_list(&merge_hunks(
        &terms,
        store.merge_options(),
    )))))
}

/// The unresolved hunks of `merged`, the merge hunks of a conflict between
/// regular files, in file order ([`ConflictHunk`]).
///
/// Each hunk is simplified on its own, so a side that equals a base cancels
/// with it ([`Merge::simplify`]). A merge of more than two parents keeps such
/// a side in a hunk where its parent changed only other lines of the file,
/// and a simplify of the whole file leaves it there.
fn hunk_list(merged: &MergeResult) -> Vec<ConflictHunk> {
    let MergeResult::Conflict(hunks) = merged else {
        return Vec::new();
    };
    hunks
        .iter()
        .map(Merge::simplify)
        .filter(|hunk| !hunk.is_resolved())
        .map(|hunk| {
            let mut bases: Vec<Vec<u8>> = hunk.removes().map(|term| term.to_vec()).collect();
            let mut sides: Vec<Vec<u8>> = hunk.adds().map(|term| term.to_vec()).collect();
            bases.sort_unstable();
            sides.sort_unstable();
            ConflictHunk { bases, sides }
        })
        .collect()
}

/// Whether spec R11 keeps the duplicate's value at a path where the plain
/// merge of the release in hand's own parents leaves `answered`.
///
/// It does where that conflict has no unresolved hunk; where `release`, the
/// release's own value there when it is a conflict, leaves the same hunks,
/// every one still open with the sides it had, so the release answers none
/// of them; and where `new`, the new parents' plain merge there when it is a
/// conflict, leaves the same hunks, so the same conflict comes back. A count
/// of a side that changed makes the hunks differ.
fn keeps_duplicate(
    answered: &ConflictHunks,
    release: Option<&ConflictHunks>,
    new: Option<&ConflictHunks>,
) -> bool {
    matches!(answered, ConflictHunks::Text(hunks) if hunks.is_empty())
        || release == Some(answered)
        || new == Some(answered)
}

/// Whether spec R11 leaves a release log to the union: the union, applied
/// as [`super::union_release_logs`] applies it, joins `terms`, the conflict
/// of the plain merge of the release in hand's own parents, with `earlier`,
/// the release in hand's own content of the log, as the earlier release.
fn left_to_union<T: AsRef<[u8]>>(
    terms: &Merge<T>,
    earlier: Option<&[u8]>,
    options: &MergeOptions,
) -> bool {
    changelog_union::union_of(terms, earlier, options).is_some()
}

/// Whether `release`, the release's file, changed lines outside the conflict
/// hunks of `hunks`, the merge hunks of the plain merge of its own parents,
/// in file order.
///
/// The lines of the resolved hunks, each conflict hunk a position between
/// them, are line-diffed against the file ([`ContentDiff::by_line`]). A
/// resolved line deleted or changed, or a line inserted at a position no
/// conflict hunk holds, is such a change. A line inserted at a conflict
/// hunk's position is part of its resolution, at either edge of its region
/// too, and so is a conflict marker the file still holds there.
fn changed_outside<T: AsRef<[u8]>>(hunks: &[Merge<T>], release: &[u8]) -> bool {
    let mut resolved = Vec::new();
    let mut positions = Vec::new();
    for hunk in hunks {
        if let Some(text) = hunk.as_resolved() {
            resolved.extend_from_slice(text.as_ref());
            continue;
        }
        positions.push(resolved.len());
    }
    ContentDiff::by_line([resolved.as_slice(), release])
        .hunk_ranges()
        .filter(|range| range.kind == DiffHunkKind::Different)
        .any(|range| {
            range
                .ranges
                .first()
                .is_some_and(|ours| !ours.is_empty() || !positions.contains(&ours.start))
        })
}

/// The file `value` holds at `path` as jj materializes it
/// ([`materialize_tree_value`]): its text, with jj's conflict markers in its
/// diff style where it is still a conflict ([`conflict_text`]), or no text
/// where it is absent; `None` for a value that is no file, and for one the
/// store refuses to read.
fn materialized_file(
    store: &jj_lib::store::Store,
    path: &RepoPath,
    value: MergedTreeValue,
) -> Result<Option<Vec<u8>>, JjError> {
    Ok(match materialized(store, path, value)? {
        MaterializedTreeValue::Absent => Some(Vec::new()),
        MaterializedTreeValue::File(mut file) => {
            Some(block_on(file.read_all(path)).map_err(|error| store_error(&error))?)
        }
        MaterializedTreeValue::FileConflict(file) => {
            Some(conflict_text(store, &file, ConflictMarkerStyle::Diff).0)
        }
        _ => None,
    })
}

/// `value` padded with absent terms to `sides`, the number of sides of the
/// tree it is set in, where it is a conflict with fewer.
/// [`MergedTreeBuilder`] sets each term of a conflict in its own tree, and
/// a tree that gets no term of its own keeps its old value at the path.
fn padded(mut value: MergedTreeValue, sides: usize) -> MergedTreeValue {
    if !value.is_resolved() {
        value.pad_to(sides, &None);
    }
    value
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        reason = "a failed fixture setup or assertion is the test failure"
    )]

    use jj_lib::backend::{SymlinkId, TreeValue};
    use jj_lib::files::{MergeResult, merge_hunks};
    use jj_lib::merge::Merge;

    use super::{
        BroughtLines, ConflictHunks, changed_outside, counted_lines, hunk_list, keeps_duplicate,
        left_to_union, padded,
    };
    use crate::changelog_union::test_support::merge_options;
    use crate::own_lines::{Lost, LostKind, Placed, PlacedLost};

    /// The probe's `f`: thirty lines `let lN = N`, with each line of
    /// `changed` set to its value.
    fn probe_file(changed: &[(usize, &str)]) -> String {
        use std::fmt::Write as _;
        let mut file = String::new();
        for line in 1..=30 {
            let value = changed
                .iter()
                .find(|(at, _)| *at == line)
                .map_or_else(|| line.to_string(), |(_, value)| (*value).to_owned());
            writeln!(file, "let l{line} = {value}").expect("write to a String");
        }
        file
    }

    /// The conflict a merge of `terms`, in jj's order of sides and bases,
    /// leaves, as spec R11 compares it.
    fn text_conflict(terms: &[&str]) -> ConflictHunks {
        ConflictHunks::Text(hunk_list(&merge_hunks(
            &Merge::from_vec(terms.to_vec()),
            &merge_options(),
        )))
    }

    /// The terms of a conflict of symlinks, one for each of `targets`, which
    /// no regular-file reading takes.
    fn symlinks(targets: &[u8]) -> Vec<Option<TreeValue>> {
        targets
            .iter()
            .map(|target| Some(TreeValue::Symlink(SymlinkId::new(vec![*target]))))
            .collect()
    }

    #[test]
    fn a_side_equal_to_the_base_inside_a_hunk_cancels_so_a_three_parent_merge_matches() {
        // Given (the probe's case 1d): D and A conflict at lines 5 and 6,
        // and P changes only line 30, so its side equals the base inside
        // that hunk.
        let base = probe_file(&[]);
        let d = probe_file(&[(5, "compare_fix ()"), (6, "d809_only ()")]);
        let a = probe_file(&[(5, "compare_fix ()")]);
        let p = probe_file(&[(30, "extra ()")]);

        // When: the hunks of the merge of D and A, and of D, A and P, are
        // read.
        let answered = text_conflict(&[&d, &base, &a]);
        let with_p = text_conflict(&[&d, &base, &a, &base, &p]);

        // Then: they are the same one hunk.
        assert!(matches!(&answered, ConflictHunks::Text(hunks) if hunks.len() == 1));
        assert_eq!(with_p, answered);
    }

    #[test]
    fn a_side_whose_count_changes_makes_the_hunks_differ_and_an_equal_repeat_does_not() {
        // Given (K14 21): `f` is x on the base; A and P, a cherry-pick of A,
        // write a; D writes d.
        let with_pick = text_conflict(&["a\n", "x\n", "a\n", "x\n", "d\n"]);

        // Then: without P, a's side counts once, so the hunks differ.
        assert_ne!(text_conflict(&["a\n", "x\n", "d\n"]), with_pick);
        // And: a member that leaves `f` at the base changes nothing.
        assert_eq!(
            text_conflict(&["a\n", "x\n", "a\n", "x\n", "d\n", "x\n", "x\n"]),
            with_pick
        );
    }

    #[test]
    fn a_conflict_that_is_not_between_regular_files_is_compared_whole() {
        // Given: a conflict of symlinks.
        let whole = |targets: &[u8]| ConflictHunks::Whole(Merge::from_vec(symlinks(targets)));
        let ours = whole(&[1, 0, 2]);

        // Then: the same terms in another order are the same conflict; a
        // side that repeats, or a text conflict, is another.
        assert_eq!(whole(&[2, 0, 1]), ours);
        assert_ne!(whole(&[1, 0, 1, 0, 2]), ours);
        assert_ne!(text_conflict(&["a\n", "x\n", "d\n"]), ours);
    }

    #[test]
    fn a_conflicted_value_is_padded_to_the_sides_of_the_tree_it_is_set_in() {
        let conflict = Merge::from_vec(symlinks(&[1, 0, 2]));

        // A conflict set over a tree with more sides gets an absent term for
        // each side it lacks.
        let mut terms = symlinks(&[1, 0, 2]);
        terms.extend([None, None]);
        assert_eq!(padded(conflict.clone(), 3), Merge::from_vec(terms));
        // A conflict set over a resolved tree, which has fewer sides, stays
        // as it is: the builder pads the tree.
        assert_eq!(padded(conflict.clone(), 1), conflict);
        // A resolved value is set in every tree as it is.
        let resolved = Merge::normal(TreeValue::Symlink(SymlinkId::new(vec![3])));
        assert_eq!(padded(resolved.clone(), 3), resolved);
    }

    #[test]
    fn the_line_test_names_a_change_outside_the_conflict_and_not_its_resolution() {
        // Given: the conflict of D and A at lines 5 and 6 of the probe's
        // `f`.
        let base = probe_file(&[]);
        let d = probe_file(&[(5, "compare_fix ()"), (6, "d809_only ()")]);
        let a = probe_file(&[(5, "compare_fix ()")]);
        let MergeResult::Conflict(hunks) = merge_hunks(
            &Merge::from_vec(vec![d.as_str(), base.as_str(), a.as_str()]),
            &merge_options(),
        ) else {
            panic!("D and A do not conflict");
        };
        let changed = |file: &str| changed_outside(&hunks, file.as_bytes());

        // Then: the resolution, a line inserted at the edge of its region,
        // and conflict markers still at its place are not a change outside
        // it.
        assert!(!changed(&d));
        assert!(!changed(&d.replace(
            "let l6 = d809_only ()\n",
            "let l6 = d809_only ()\nlet edge = 0\n"
        )));
        assert!(!changed(&base.replace(
            "let l5 = 5\nlet l6 = 6\n",
            "<<<<<<< conflict 1 of 1\n+++++++ side #1\nlet l5 = a ()\n------- base\nlet l5 = \
             5\n+++++++ side #2\nlet l5 = b ()\n>>>>>>> conflict 1 of 1 ends\n"
        )));
        // And: a resolved line deleted, one changed, and a line inserted
        // away from the conflict are.
        assert!(changed(&d.replace("let l10 = 10\n", "")));
        assert!(changed(&d.replace("let l10 = 10\n", "let l10 = ten\n")));
        assert!(changed(
            &d.replace("let l20 = 20\n", "let l20 = 20\nlet e = hand ()\n")
        ));
    }

    #[test]
    fn the_rule_keeps_the_duplicate_where_the_release_answers_nothing_or_the_conflict_returns() {
        // Given: A and D conflict at lines 5 and 6 (h1) and at line 20 (h2).
        let base = probe_file(&[]);
        let a = probe_file(&[(5, "compare_fix ()"), (20, "a_twenty ()")]);
        let d = probe_file(&[
            (5, "compare_fix ()"),
            (6, "d809_only ()"),
            (20, "d_twenty ()"),
        ]);
        let answered = text_conflict(&[&a, &base, &d]);
        let open = text_conflict(&[&a, &base, &d]);
        // The release resolved h1 to D's text and left h2 open.
        let in_part = text_conflict(&[
            &probe_file(&[
                (5, "compare_fix ()"),
                (6, "d809_only ()"),
                (20, "a_twenty ()"),
            ]),
            &probe_file(&[(5, "compare_fix ()"), (6, "d809_only ()")]),
            &d,
        ]);
        // The release left both open but changed A's side in h1.
        let side_changed = text_conflict(&[
            &probe_file(&[(5, "compare_fix (2)"), (20, "a_twenty ()")]),
            &base,
            &d,
        ]);

        // Then: after an edit wrote the plain merge, the old merge leaves no
        // hunk, so a returning conflict (`drop D`, then `include D`) is
        // written as the duplicate writes it.
        assert!(keeps_duplicate(
            &ConflictHunks::Text(Vec::new()),
            None,
            Some(&answered)
        ));
        // And: a release that leaves every hunk open with its sides answers
        // none of them.
        assert!(keeps_duplicate(&answered, Some(&open), None));
        // And: a release that resolved a part, or changed a side of a hunk it
        // left open, follows the rule: the plain merge where the conflict
        // is gone, the duplicate where the same hunks come back.
        assert!(!keeps_duplicate(&answered, Some(&in_part), None));
        assert!(keeps_duplicate(&answered, Some(&in_part), Some(&answered)));
        assert!(!keeps_duplicate(&answered, Some(&side_changed), None));
        // And: a resolution meets other hunks, or a side's count that
        // changed, with the plain merge.
        let other = text_conflict(&[&a, &base, &probe_file(&[(5, "other_fix ()")])]);
        assert!(!keeps_duplicate(&answered, None, Some(&other)));
        assert!(!keeps_duplicate(
            &text_conflict(&["a\n", "x\n", "a\n", "x\n", "d\n"]),
            None,
            Some(&text_conflict(&["a\n", "x\n", "d\n"]))
        ));
    }

    #[test]
    fn a_release_log_is_left_to_the_union_only_where_the_union_joins_its_conflict() {
        let options = merge_options();
        let base = "# Changelog\n## [1.0.0]\n- old\n";

        // Each side only inserts its own block: the union joins them.
        let x = "# Changelog\n## [1.1.0-x]\n- x\n## [1.0.0]\n- old\n";
        let y = "# Changelog\n## [1.1.0-y]\n- y\n## [1.0.0]\n- old\n";
        assert!(left_to_union(
            &Merge::from_vec(vec![x, base, y]),
            None,
            &options
        ));

        // D holds A's entry and also changes the heading below it: the union
        // declines, so the rule reads the log.
        let a = "# Changelog\n## [Unreleased]\n- a entry\n## [1.0.0]\n- old\n";
        let d = "# Changelog\n## [Unreleased]\n- a entry\n- d entry\n## [1.0.0] - fork\n- old\n";
        assert!(!left_to_union(
            &Merge::from_vec(vec![d, base, a]),
            Some(d.as_bytes()),
            &options
        ));

        // A block with no final newline joins only where it ends the file,
        // so the answer depends on the order the earlier release gives the
        // sides: a log joined in one order can read as declined in another.
        let blocks = Merge::from_vec(vec!["x1\n", "", "y1"]);
        assert!(left_to_union(&blocks, None, &options));
        assert!(!left_to_union(&blocks, Some(b"x1\n".as_slice()), &options));
    }

    /// A line a member added to `f` at its tip's `line`, holding `text`.
    fn line(line: u32, text: &str) -> Lost {
        Lost {
            path: "f".to_owned(),
            kind: LostKind::Added,
            line: Some(line),
            text: Some(text.to_owned()),
            carried_forward: false,
            conflict: None,
        }
    }

    /// The member's lines that a version lacks, each with whether it is at
    /// a conflict there.
    fn lacks(lines: &[(&Lost, bool)]) -> Vec<PlacedLost> {
        lines
            .iter()
            .map(|&(lost, at_conflict)| PlacedLost {
                lost: lost.clone(),
                at_conflict,
            })
            .collect()
    }

    /// A version that lacks `lines` ([`lacks`]).
    fn version(lines: &[(&Lost, bool)]) -> Placed {
        Placed::Lines {
            lost: lacks(lines),
            word_step_skipped: Vec::new(),
        }
    }

    /// The tip lines of what the check counts.
    fn counted(
        candidate: &[(&Lost, bool)],
        (release, plain): (&Placed, &Placed),
        brought: &BroughtLines,
    ) -> Vec<Option<u32>> {
        counted_lines(lacks(candidate), (release, plain), brought)
            .into_iter()
            .map(|lost| lost.line)
            .collect()
    }

    #[test]
    fn a_line_lost_away_from_a_conflict_that_both_versions_hold_counts() {
        let five = line(5, "five");
        let holds = version(&[]);

        // A line lost in the candidate at no conflict, held by the release in
        // hand and by the plain merge, counts.
        let lost = counted(&[(&five, false)], (&holds, &holds), &BroughtLines::Nothing);
        assert_eq!(lost, [Some(5)]);
        // So does one the plain merge has at a conflict.
        let at_conflict = version(&[(&five, true)]);
        let lost = counted(
            &[(&five, false)],
            (&holds, &at_conflict),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, [Some(5)]);
        // And one the release in hand has at a conflict.
        let lost = counted(
            &[(&five, false)],
            (&at_conflict, &holds),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, [Some(5)]);
        // A version that gives no place has every line at a conflict.
        let lost = counted(
            &[(&five, false)],
            (&Placed::AtConflict, &Placed::AtConflict),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, [Some(5)]);
    }

    #[test]
    fn a_line_the_plain_merge_lacks_with_no_conflict_does_not_count() {
        let (five, nine) = (line(5, "five"), line(9, "nine"));
        let holds = version(&[]);

        // The plain merge lacks the line at no conflict there, though it has
        // another line of the same file at a conflict: nothing counts.
        let plain = version(&[(&five, false), (&nine, true)]);
        let lost = counted(&[(&five, false)], (&holds, &plain), &BroughtLines::Whole);
        assert_eq!(lost, Vec::<Option<u32>>::new());
    }

    #[test]
    fn a_line_at_a_conflict_in_the_candidate_does_not_count_while_another_does() {
        let (five, nine) = (line(5, "five"), line(9, "nine"));
        let holds = version(&[]);

        let lost = counted(
            &[(&five, true), (&nine, false)],
            (&holds, &holds),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, [Some(9)]);
    }

    #[test]
    fn a_line_the_release_in_hand_lacked_counts_only_where_the_edit_brings_it_in() {
        let five = line(5, "five");
        let (lacked, holds) = (version(&[(&five, false)]), version(&[]));
        let lost = |brought| counted(&[(&five, false)], (&lacked, &holds), &brought);

        // A line the release in hand already lacked at no conflict is not
        // this edit's loss...
        assert_eq!(lost(BroughtLines::Nothing), Vec::<Option<u32>>::new());
        // ...unless the edit brings it in: every line of an included member,
        assert_eq!(lost(BroughtLines::Whole), [Some(5)]);
        // and a line that an advance added, compared with the old tip's
        // change by path, kind and text, with counts.
        let old_once = BroughtLines::Since {
            old: vec![line(3, "five")],
            new: vec![five.clone()],
        };
        assert_eq!(lost(old_once), Vec::<Option<u32>>::new());
        let new_twice = BroughtLines::Since {
            old: vec![line(3, "five")],
            new: vec![line(2, "five"), five.clone()],
        };
        assert_eq!(lost(new_twice), [Some(5)]);
    }

    #[test]
    fn of_two_equal_lines_the_one_the_release_in_hand_held_counts() {
        // Given: a member whose change adds the same text at lines 5 and 7;
        // the release in hand lacked line 5.
        let (five, seven) = (line(5, "same"), line(7, "same"));
        let (release, holds) = (version(&[(&five, false)]), version(&[]));

        // Then: the candidate's loss of line 7 counts, and of line 5 does not.
        let lost = counted(
            &[(&seven, false)],
            (&release, &holds),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, [Some(7)]);
        let lost = counted(
            &[(&five, false)],
            (&release, &holds),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, Vec::<Option<u32>>::new());
    }

    #[test]
    fn a_line_past_the_word_limit_is_the_same_line_as_an_added_one() {
        // Given: the candidate loses line 5 in a range the word step did not
        // follow, and the release in hand lacked it as an added line.
        let mut word_limit = line(5, "five");
        word_limit.kind = LostKind::WordLimit;
        let five = line(5, "five");
        let (release, holds) = (version(&[(&five, false)]), version(&[]));

        // Then: it is the line the release in hand lacked, so nothing counts.
        let lost = counted(
            &[(&word_limit, false)],
            (&release, &holds),
            &BroughtLines::Nothing,
        );
        assert_eq!(lost, Vec::<Option<u32>>::new());
    }
}
