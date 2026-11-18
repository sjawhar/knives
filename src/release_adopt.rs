//! `knives release adopt`: record a release finished by hand as its newest
//! knives write.
//!
//! A person who resolves a release's conflicts, renumbers a release log or
//! repairs the release commit leaves a commit knives did not write, and every
//! write over it refuses until it is recorded. Adopting it is one edit event,
//! which every reader of knives' own writes already reads, after the checks
//! that make it that release finished: its members are a recorded write's
//! (beside any earlier write of the name a fast-forward push added), no
//! conflicts, and a reason for any change beyond the conflicts the write left.

use std::collections::BTreeMap;

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::commands::release;
use knives::forge::github::CliForge;
use knives::ids::{BookmarkRef, BranchName, ChangeId, CommitId, RemoteName, RepoName};
use knives::jj::{Repo, ResolutionBaseline, ResolutionChanges};
use knives::ledger::{Draft, Entry, FixChange, FixFile, Ledger, RecordedParent, inline_human_text};
use knives::release_model::{
    ADOPTED_BEYOND, ADOPTED_RESOLVED, EarlierWrites, PreviousRelease, RecordedWrite,
    ReleaseReading, carrier_label, matched_write, previous_release_label, recorded_members,
    recorded_write_events,
};

use super::release_edit::{
    composition_sources, edit_event_text, recorded_parents, refuse_plan_problems,
    refuse_unresolved_trunk, report_abandoned_earlier, report_hidden,
};
use super::scribe_for;

/// Adopt the commit `reference` holds, or the release in hand when there is
/// no reference, as the name's newest write. `why` is the reason for a change
/// beyond the conflicts the replaced write left.
pub(crate) fn run_release_adopt(
    fork: &Fork<'_>,
    extra_consumers: &[&std::path::Path],
    (reference, why): (Option<&str>, Option<&str>),
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let consumers = release::ConsumerInputs {
        slugs: &fork.entry.consumers,
        locals: &locals,
        forge: &forge,
        cache_root: cache_root.as_deref(),
        heads: &heads,
    };
    let plan = release::pulled_plan(fork, bound.registry, &consumers)?;
    if refuse_plan_problems(repo, &plan, "adopt") {
        return Ok(Exit::Incomplete);
    }
    let Some(reference) = reference.map(str::to_owned).or(plan.release) else {
        println!("{repo}: no release to adopt; cut one first");
        return Ok(Exit::Incomplete);
    };
    // The plan names a release held only on a remote as `name@remote`; the
    // name is what the ledger records writes under.
    let name = BookmarkRef::parse(&reference).branch().as_str().to_owned();
    if let Some(exit) = refuse_other_series(fork, &name) {
        return Ok(exit);
    }
    // The trunk bounds what adopt measures: each member's lines are counted
    // from its fork point with it (or with a stacked release's previous
    // release), the check that catches a resolution dropping a member's work,
    // and a stacked release is read along its first-parent line down to it.
    // So no adopt is recorded without it.
    let opened = Repo::open(&fork.checkout.path)?;
    if refuse_unresolved_trunk(&opened, repo, fork.entry) {
        return Ok(Exit::Incomplete);
    }
    let Some(held) = Held::read(fork, opened, name)? else {
        return Ok(Exit::Incomplete);
    };
    adopt(fork, &held, why, bound)
}

/// Refuse, naming the series that owns it, a name that is not a release of
/// the selected series: `Some(Usage)` after refusing. A release's writes are
/// recorded under its own series, so another series' name is a mistyped
/// command, not a release to measure.
fn refuse_other_series(fork: &Fork<'_>, name: &str) -> Option<Exit> {
    let entry = fork.entry;
    if entry.release_scheme().owns(name) {
        return None;
    }
    let shown = inline_human_text(name);
    match entry
        .all_series()
        .into_iter()
        .find(|series| series.release_scheme().owns(name))
    {
        Some(owner) => eprintln!(
            "{}: {shown} is a release of the {}, not of the {}; adopt it with {}",
            fork.name,
            owner.series_label(),
            entry.series_label(),
            owner.selected_series.as_ref().map_or_else(
                || "no --series".to_owned(),
                |series| format!("--series {series}")
            )
        ),
        None => eprintln!(
            "{}: {shown} is not a release of the {}, whose releases are named {}",
            fork.name,
            entry.series_label(),
            entry.release_scheme().cut_shape()
        ),
    }
    Some(Exit::Usage)
}

/// The release being adopted, as this repository holds it.
struct Held {
    name: String,
    opened: Repo,
    /// The commit to adopt: the local bookmark's, or, where there is no local
    /// bookmark, the publish remote's.
    commit: CommitId,
    /// The commit's own parents: what a release outside a stacked series
    /// lists. A stacked release lists its reading instead ([`Self::reading`]).
    parents: Vec<CommitId>,
    /// The publish remote's commit of the name, in jj's view.
    published: Option<CommitId>,
    /// Every bookmark, read once the name is held here, which a stacked
    /// series reads the commit with ([`release::release_reading`]).
    tips: knives::detect::BookmarkTips,
}

impl Held {
    /// Read the commit `name` holds in `opened`, or `None` after saying why
    /// there is no one commit to adopt.
    ///
    /// A name with no local bookmark is fetched first: a release somebody
    /// else published is held here only as the publish remote's ref, and
    /// that ref is as old as the last fetch.
    fn read(fork: &Fork<'_>, mut opened: Repo, name: String) -> anyhow::Result<Option<Self>> {
        let (repo, path) = (&fork.name, &fork.checkout.path);
        let shown = inline_human_text(&name);
        let local = opened.local_bookmark_tip(&name);
        if local.is_none() {
            let reference = BookmarkRef::Local(BranchName::new(name.as_str()));
            if let Some((_, targets)) = opened
                .conflicted_bookmarks()?
                .into_iter()
                .find(|(conflicted, _)| *conflicted == reference)
            {
                println!(
                    "{repo}: {shown} is divergent ({} targets), so there is no one commit to \
                     adopt; resolve its divergence first",
                    targets.len()
                );
                return Ok(None);
            }
            knives::jj::fetch_all(path)?;
            opened = Repo::open(path)?;
        }
        let remote = fork.entry.publish_remote();
        let tips = opened.bookmark_tips()?;
        let published = tips
            .get(&BookmarkRef::Remote {
                branch: BranchName::new(name.as_str()),
                remote: RemoteName::new(remote),
            })
            .cloned();
        let Some(commit) = local.or_else(|| published.clone()) else {
            println!(
                "{repo}: {shown} has no local bookmark, and {remote} holds no {shown} after a \
                 fetch; there is no commit to adopt"
            );
            return Ok(None);
        };
        let parents = opened.parent_commits(commit.as_str())?;
        Ok(Some(Self {
            name,
            opened,
            commit,
            parents,
            published,
            tips,
        }))
    }

    fn shown_name(&self) -> String {
        inline_human_text(&self.name)
    }

    /// The commit as the release in hand `in_hand_write` reads, `writes` read
    /// with its publish remote's commit.
    fn in_hand<'a>(
        &'a self,
        ledger: &'a [Entry],
        writes: &'a EarlierWrites,
    ) -> release::InHand<'a> {
        release::InHand {
            repo: &self.opened,
            ledger,
            name: &self.name,
            commit: &self.commit,
            writes,
            published: self.published.as_ref(),
            tips: &self.tips,
        }
    }

    /// How the commit stands to the writes `ledger` records under its name
    /// ([`release::in_hand_write`]), read as `entry`'s series reads it.
    fn standing(
        &self,
        entry: &knives::config::RepoEntry,
        (ledger, writes): (&[Entry], &EarlierWrites),
    ) -> anyhow::Result<release::InHandWrite> {
        Ok(release::in_hand_write(
            &self.in_hand(ledger, writes),
            entry,
        )?)
    }

    /// How the commit reads in `fork`'s series ([`release::release_reading`]):
    /// a stacked release as its previous release and the members on its
    /// first-parent line, never its literal parents; any other release as
    /// its own parents, its carriers apart. `None` after saying why a stacked
    /// release cannot be read: a commit with one parent between it and its
    /// previous release, which no member explains.
    fn reading(
        &self,
        fork: &Fork<'_>,
        (ledger, writes): (&[Entry], &EarlierWrites),
    ) -> anyhow::Result<Option<ReleaseReading>> {
        let reading = release::release_reading(
            &self.opened,
            fork.entry,
            (ledger, &self.tips),
            (writes, &self.commit),
        )?;
        if let Err(unexplained) = &reading {
            // The previous release's name is a ledger subject or a ref.
            println!(
                "{}: {} at {} is not adopted: {}",
                fork.name,
                self.shown_name(),
                self.commit.short(),
                inline_human_text(&unexplained.to_string())
            );
        }
        Ok(reading.ok())
    }
}

/// The write the adopted commit replaces: the recorded write it equals, when
/// it equals an older one, and otherwise the newest recorded write whose
/// members are its own ([`matched_write`]).
struct Replaced {
    write: RecordedWrite,
    /// Whether this repository holds the write's commit. A write made on
    /// another machine and never pushed is not here.
    here: bool,
    /// Whether the adopted commit is this write, or it re-signed or
    /// metaedited: an older recorded write adopted as the newest.
    equal: bool,
    /// Whether its event says knives could not check the release it was
    /// written over (`; unchecked:`): nothing measured that write, so a
    /// resolution is measured against an automerge instead.
    unchecked: bool,
    /// What the write composes, where this repository holds it
    /// ([`release::write_composition`]): its own parents less the earlier
    /// writes of the name among them, which a rebuild of it leaves out
    /// ([`ResolutionBaseline::Rebuilt`]), or a stacked write's previous
    /// release and members. A rebuild measures the write's own change
    /// against their merge, so a write made as a chain of merges keeps what
    /// each inner merge resolved.
    composed: Vec<CommitId>,
    /// The parent set its event records, its own carriers left out, each
    /// parent with every branch at it then, which `include` and `advance`
    /// match a member by once its bookmark has moved on.
    parents: Vec<RecordedParent>,
}

impl Replaced {
    /// The write `held` replaces, or `None` when no recorded write has its
    /// `members` ([`release::Unchecked::NoMatch`], already refused).
    ///
    /// `members` are the adopted commit's as its series reads them
    /// ([`Held::reading`]): a stacked release's previous release is never
    /// one, as it is no recorded write's member ([`recorded_members`]).
    ///
    /// An event an older knives wrote has no `parents`; the set is then built
    /// as a write builds it, from the write's own parents where this
    /// repository holds it and from the event's evidence where it does not.
    fn read(
        held: &Held,
        entry: &knives::config::RepoEntry,
        (ledger, writes): (&[Entry], &EarlierWrites),
        members: &[CommitId],
    ) -> anyhow::Result<Option<Self>> {
        let opened = &held.opened;
        let mut equal = None;
        for (event, write) in recorded_write_events(ledger, &held.name) {
            if opened.is_same_or_metadata_rewrite(&write.commit, &held.commit)? {
                equal = Some((event, write));
                break;
            }
        }
        let (event, write, recorded, equal) = match equal {
            Some((event, write)) => {
                let recorded = recorded_members(opened, (&held.name, event, &write), writes)?;
                (event, write, recorded, true)
            }
            None => match matched_write(opened, (ledger, &held.name), writes, members)? {
                Some(matched) => (matched.event, matched.write, matched.members, false),
                None => return Ok(None),
            },
        };
        let parents = if event.parents.is_empty() {
            let commits: Vec<CommitId> = recorded
                .iter()
                .map(|parent| CommitId::new(parent.commit.as_str()))
                .collect();
            recorded_parents(opened, entry, &commits)?
        } else {
            recorded
        };
        let here = opened.resolve_commit(write.commit.as_str()).is_ok();
        let composed = if here {
            release::write_composition(
                opened,
                entry,
                (ledger, &held.tips),
                (writes, &write.commit),
            )?
        } else {
            Vec::new()
        };
        Ok(Some(Self {
            here,
            equal,
            unchecked: knives::release_model::unchecked_mark(event).is_some(),
            write,
            composed,
            parents,
        }))
    }

    /// Whether the write is the baseline a resolution is measured against:
    /// it is here, and knives checked the release it was written over.
    /// Otherwise the baseline is an automerge of what the commit composes.
    const fn is_baseline(&self) -> bool {
        self.here && !self.unchecked
    }

    /// The baseline for a commit that composes `composition`, and how a
    /// reader is told it: the write itself, when the commit is it or a copy
    /// of it; the write rebuilt on `composition`, the tree a write onto it
    /// holds, when the write is the baseline ([`Self::is_baseline`]);
    /// otherwise a fresh automerge of `composition`, saying why.
    ///
    /// `composition` is the commit's members, or for a stacked release its
    /// `previous` release, then its members
    /// ([`ReleaseReading::composition`]): a stacked release holds its
    /// previous release whole, so its baseline merges that release and its
    /// members.
    fn baseline<'a>(
        &'a self,
        (previous, composition): (Option<&PreviousRelease>, &'a [CommitId]),
    ) -> (ResolutionBaseline<'a>, String) {
        if self.is_baseline() && self.equal {
            return (
                ResolutionBaseline::Written(&self.write.commit),
                self.described(),
            );
        }
        // What the commit composes, as a reader is told it: a previous
        // release is never a member.
        let composed = || {
            previous.map_or_else(
                || format!("its {} member parent(s)", composition.len()),
                |previous| {
                    format!(
                        "its previous release {} and its {} member(s)",
                        previous.commit.short(),
                        composition.len().saturating_sub(1)
                    )
                },
            )
        };
        if self.is_baseline() {
            // Rebuilt on its own composition, a write is its own tree, so it
            // is named as itself.
            let described = if self.composed == composition {
                self.described()
            } else {
                format!("{} rebuilt on {}", self.described(), composed())
            };
            return (
                ResolutionBaseline::Rebuilt {
                    write: &self.write.commit,
                    composed: &self.composed,
                    members: composition,
                },
                described,
            );
        }
        let reason = if self.here {
            format!(
                "knives could not check the release {} was written over",
                self.described()
            )
        } else {
            format!("this repository does not hold {}", self.described())
        };
        (
            ResolutionBaseline::Automerge(composition),
            format!(
                "an automerge of {} with the release-log union, because {reason}",
                composed()
            ),
        )
    }

    /// `the recorded edit 0123456789ab`.
    fn described(&self) -> String {
        release::described_write(&self.write)
    }
}

/// Whether `held` already is the newest recorded write of its name, saying
/// so: the write itself, or the write re-signed or metaedited, which audit
/// already reads as it. An older recorded write is recorded as the newest,
/// and so is a newest one written over a release knives could not check
/// (its event marked `; unchecked:`), which stays unchecked until adopted.
fn is_newest(repo: &RepoName, held: &Held, ledger: &[Entry]) -> anyhow::Result<bool> {
    let Some((event, newest)) = recorded_write_events(ledger, &held.name).next() else {
        return Ok(false);
    };
    if knives::release_model::unchecked_mark(event).is_some() {
        return Ok(false);
    }
    let (shown, at) = (held.shown_name(), held.commit.short());
    let described = release::described_write(&newest);
    if held.commit == newest.commit {
        println!("{repo}: {shown} is at {at}, {described}; nothing to adopt");
        return Ok(true);
    }
    if held
        .opened
        .is_same_or_metadata_rewrite(&newest.commit, &held.commit)?
    {
        println!(
            "{repo}: {shown} is at {at}, {described} re-signed or metaedited (same change, parents \
             and tree); nothing to adopt"
        );
        return Ok(true);
    }
    Ok(false)
}

/// What every check reads: whose release, the commit to adopt, the ledger and
/// the write it replaces.
struct Adopting<'a, 'f> {
    fork: &'a Fork<'f>,
    held: &'a Held,
    ledger: &'a [Entry],
    replaced: &'a Replaced,
}

impl Adopting<'_, '_> {
    /// Refuse, saying why, a commit that is not finished
    /// ([`refuse_unfinished_commit`]), and a hand edit of a published release
    /// the publish remote does not hold. `true` after refusing.
    fn refuse_unfinished(&self) -> anyhow::Result<bool> {
        let held = self.held;
        if refuse_unfinished_commit(
            &self.fork.name,
            &held.opened,
            (&held.name, &held.commit),
            ("adopt", "adopted"),
        )? {
            return Ok(true);
        }
        self.refuse_published()
    }

    /// Refuse a hand edit of a published release while the publish remote
    /// does not hold it: consumers were given the published commit, and
    /// adopting would record under its name a release they were never given.
    /// Published is read as the edit verbs read it, over the commit and the
    /// publish remote's, so the cut and every write print the same remedy
    /// ([`release::published_hand_edit_refusal`]). `true` after refusing.
    fn refuse_published(&self) -> anyhow::Result<bool> {
        let held = self.held;
        let Some(remedy) = release::published_hand_edit_refusal(
            (&held.opened, self.ledger),
            (&held.name, &held.commit, held.published.as_ref()),
            self.fork.entry,
        )?
        else {
            return Ok(false);
        };
        println!("{}: {remedy}", self.fork.name);
        Ok(true)
    }
}

/// Refuse, saying why, a commit that is not finished, as `adopt` and `cut
/// NAME --at` read one: it still has conflicts, or its conflicts cannot be
/// read, or a workspace has it checked out, where an edit would change it
/// once it is recorded. `name` is the release it would be recorded under;
/// `(verb, done)` name what is refused, as `("adopt", "adopted")`. `true`
/// after refusing.
pub(crate) fn refuse_unfinished_commit(
    repo: &RepoName,
    opened: &Repo,
    (name, commit): (&str, &CommitId),
    (verb, done): (&str, &str),
) -> anyhow::Result<bool> {
    // The release is a name read from the repository.
    let (shown, at) = (inline_human_text(name), commit.short());
    let conflicted = match opened.conflicted_files(commit.as_str()) {
        Ok(conflicted) => conflicted,
        Err(error) => {
            println!(
                "{repo}: could not read whether {shown} at {at} still has conflicts ({}), so it \
                 is not {done}",
                inline_human_text(&error.to_string())
            );
            return Ok(true);
        }
    };
    if !conflicted.is_empty() {
        println!(
            "{repo}: {shown} at {at} still has {} conflicted file(s), so it is not resolved yet; \
             resolve them, then {verb}:",
            conflicted.len()
        );
        // The paths are the repository's, each whole.
        for file in &conflicted {
            println!("    {}", inline_human_text(file));
        }
        return Ok(true);
    }
    let change = opened.change_id_of(commit.as_str())?;
    let checked_out: Vec<String> = opened
        .workspaces()?
        .into_iter()
        .filter(|(_, working)| *working == change)
        .map(|(workspace, _)| inline_human_text(workspace.as_str()))
        .collect();
    if !checked_out.is_empty() {
        println!(
            "{repo}: {shown} at {at} is the working copy of workspace {}; run `jj new` there \
             first, so editing in that workspace cannot change the {done} commit",
            checked_out.join(", ")
        );
        return Ok(true);
    }
    Ok(false)
}

/// What adopting measured: everything the event and the report say.
struct Adoption<'a> {
    change: ChangeId,
    /// The previous release a stacked release is built on, which the event
    /// records under its label ([`previous_release_label`]), never a member.
    previous: Option<PreviousRelease>,
    /// The parents that are earlier writes of the name.
    carriers: Vec<CommitId>,
    /// Each parent with the name a reader knows it by: the commit's own
    /// parents, or for a stacked release its previous release, its members,
    /// then its carriers.
    provenance: Vec<(String, CommitId)>,
    /// What the commit was measured against, as a reader is told.
    baseline: String,
    changes: ResolutionChanges,
    why: Option<&'a str>,
    /// The counts, one per member and changed file the baseline holds lines
    /// of it added.
    lost: Vec<LostLines>,
    /// Whether the baseline is a recorded write ([`Replaced::is_baseline`]).
    /// An automerge stands in for a write this repository does not hold or
    /// one marked unchecked, so its counts are printed and never make
    /// findings.
    against_write: bool,
}

impl Adoption<'_> {
    /// A loss a reviewer missed, which makes `adopt` exit with findings
    /// ([`LostLines::is_finding`]).
    fn loses_a_member(&self) -> bool {
        self.lost
            .iter()
            .any(|lost| lost.is_finding(self.against_write))
    }
}

/// Measure the commit, as its series reads it (`reading`), against its
/// baseline, or `None` after refusing a change beyond the baseline's
/// conflicts that has no reason.
fn measure<'a>(
    adopting: &Adopting<'_, '_>,
    reading: ReleaseReading,
    why: Option<&'a str>,
) -> anyhow::Result<Option<Adoption<'a>>> {
    let (fork, held, replaced) = (adopting.fork, adopting.held, adopting.replaced);
    let entry = fork.entry;
    let composition = reading.composition();
    let ReleaseReading {
        previous,
        members,
        carriers,
    } = reading;
    let (baseline, described) = replaced.baseline((previous.as_ref(), &composition));
    let changes = knives::jj::resolution_changes(&held.opened, baseline, &held.commit)?;
    if !changes.beyond.is_empty() && why.is_none() {
        println!(
            "{}: {} at {} changes {} file(s) {described} left without a conflict, which is a \
             repair or a renumber rather than a resolution; say why with --why, which the record \
             keeps:",
            fork.name,
            held.shown_name(),
            held.commit.short(),
            changes.beyond.len()
        );
        for file in &changes.beyond {
            println!("    {}", inline_human_text(file));
        }
        return Ok(None);
    }
    // A stacked release lists its previous release first, under its label,
    // then its members, as its writes record them, then its carriers. Its
    // members are measured from their fork point with the previous release:
    // that release's own lines are no member's, so a member's deletion of
    // them is no lost line.
    let (provenance, base) = if let Some(previous) = &previous {
        let mut provenance = composition_sources(&held.opened, entry, (Some(previous), &members))?;
        provenance.extend(
            carriers
                .iter()
                .map(|carrier| (carrier_label(&held.name), carrier.clone())),
        );
        (provenance, previous.commit.clone())
    } else {
        let scheme = entry.release_scheme();
        let mut provenance = release::parent_sources(&held.opened, entry, &scheme, &held.parents)?;
        for (source, commit) in &mut provenance {
            if carriers.contains(commit) {
                *source = carrier_label(&held.name);
            }
        }
        // The trunk resolved before the release was read
        // (`refuse_unresolved_trunk`).
        let trunk = held.opened.resolve_commit(&entry.trunk_ref())?;
        (provenance, trunk)
    };
    let member_sources: Vec<(String, CommitId)> = provenance
        .iter()
        .filter(|(_, commit)| members.contains(commit))
        .cloned()
        .collect();
    let lost = lost_lines(
        &held.opened,
        (&base, &member_sources),
        (&held.commit, &changes),
    )?;
    Ok(Some(Adoption {
        change: held.opened.change_id_of(held.commit.as_str())?,
        previous,
        carriers,
        provenance,
        baseline: described,
        changes,
        why,
        lost,
        against_write: replaced.is_baseline(),
    }))
}

/// One member's added lines of one changed file that the baseline holds,
/// never none, and how many of them the resolution does not hold.
struct LostLines {
    member: String,
    file: String,
    /// The member's added lines the baseline holds: the ones a resolution
    /// can lose. A line an earlier recorded write already lost is no line
    /// here, so a loss is never charged twice.
    counted: usize,
    /// Of those, the ones the resolution holds fewer times than the baseline.
    lost: usize,
    /// Whether the baseline left the file conflicted.
    conflicted: bool,
    /// Whether the baseline held one of the blocks the member added and the
    /// resolution holds none of them ([`block_appears`]).
    dropped: bool,
}

impl LostLines {
    /// Measured against a recorded write (`against_write`), a file the
    /// baseline left without a conflict is never part of a resolution, so
    /// any lost line of it is a finding, except in a release log
    /// ([`knives::changelog_union::eligible`]), where a renumber rewrites a
    /// member's heading. In a release log or a conflicted file partial loss
    /// is the normal shape of a resolution, so the finding is a member none
    /// of whose blocks is left. Against an automerge nothing is a finding.
    fn is_finding(&self, against_write: bool) -> bool {
        if !against_write {
            false
        } else if !self.conflicted && !knives::changelog_union::eligible(&self.file) {
            self.lost > 0
        } else {
            self.dropped
        }
    }
}

/// For each file the commit changes against its baseline and each member
/// that added lines to it, the member's added non-blank lines, from its fork
/// point with `base` to its tip, that the baseline holds and the resolution
/// holds fewer times, and whether the resolution dropped every block of them
/// the baseline held.
///
/// `base` is the trunk, or a stacked release's previous release. The fork
/// point is the member's own, so a trunk that moved on since is never charged
/// to it, and neither is a previous release's own line that a member built on
/// it holds. Counted as a multiset because a release log repeats lines
/// (`### Added`) that one member adding and another keeping must not cancel.
/// Every file the commit changes is counted, a file the baseline left without
/// a conflict too: restoring such a file can drop a member's line as surely as
/// resolving a conflict can.
fn lost_lines(
    opened: &Repo,
    (base, members): (&CommitId, &[(String, CommitId)]),
    (resolution, changes): (&CommitId, &ResolutionChanges),
) -> anyhow::Result<Vec<LostLines>> {
    let mut forked = Vec::with_capacity(members.len());
    for (member, tip) in members {
        forked.push((member, tip, opened.fork_point(tip, base)?));
    }
    let mut lost = Vec::new();
    let files = changes
        .resolved
        .iter()
        .map(|file| (file, true))
        .chain(changes.beyond.iter().map(|file| (file, false)));
    for (file, conflicted) in files {
        let resolved = opened.file_bytes(resolution, file)?.unwrap_or_default();
        let baseline: Vec<&[u8]> = changes
            .baseline
            .get(file)
            .map(|sides| sides.iter().map(Vec::as_slice).collect())
            .unwrap_or_default();
        let lines = FileLines::new(&baseline, &resolved);
        for (member, tip, fork_point) in &forked {
            // A member that left the file as it found it added no line of it.
            if opened.same_path_value(fork_point, tip, file)? {
                continue;
            }
            let before = opened.file_bytes(fork_point, file)?.unwrap_or_default();
            let after = opened.file_bytes(tip, file)?.unwrap_or_default();
            let (counted, absent) = lines.counted_and_lost((&before, &after));
            if counted > 0 {
                lost.push(LostLines {
                    member: String::clone(member),
                    file: file.clone(),
                    counted,
                    lost: absent,
                    conflicted,
                    dropped: blocks_dropped(
                        (&before, &after),
                        (&baseline, &resolved),
                        knives::changelog_union::eligible(file),
                    ),
                });
            }
        }
    }
    Ok(lost)
}

/// Each non-blank line of `text`, with how many times it occurs.
fn line_counts(text: &[u8]) -> BTreeMap<&[u8], usize> {
    let mut counts = BTreeMap::new();
    for line in text.split(|byte| *byte == b'\n') {
        if !line.iter().all(u8::is_ascii_whitespace) {
            *counts.entry(line).or_insert(0) += 1;
        }
    }
    counts
}

/// The lines one changed file's baseline and resolution hold, counted once
/// for every member that added lines to it.
struct FileLines<'a> {
    /// Each non-blank line the baseline holds, as often as the side holding
    /// it most.
    kept: BTreeMap<&'a [u8], usize>,
    /// Each non-blank line the resolution holds, with how many times.
    held: BTreeMap<&'a [u8], usize>,
}

impl<'a> FileLines<'a> {
    /// A baseline the file is conflicted in holds each side's lines:
    /// `baseline` is every side's file, never the markers a materialized
    /// conflict shows.
    fn new(baseline: &[&'a [u8]], resolution: &'a [u8]) -> Self {
        let mut kept: BTreeMap<&[u8], usize> = BTreeMap::new();
        for side in baseline {
            for (line, count) in line_counts(side) {
                let most = kept.entry(line).or_insert(0);
                *most = (*most).max(count);
            }
        }
        Self {
            kept,
            held: line_counts(resolution),
        }
    }

    /// How many of the non-blank lines `after` adds to `before` (as a
    /// multiset) the baseline holds, and how many of those the resolution
    /// holds fewer times than the baseline does, each line capped at the
    /// member's own count.
    fn counted_and_lost(&self, (before, after): (&[u8], &[u8])) -> (usize, usize) {
        let (before, after) = (line_counts(before), line_counts(after));
        let mut counted = 0;
        let mut lost = 0;
        for (line, count) in after {
            let new = count.saturating_sub(before.get(line).copied().unwrap_or(0));
            let in_baseline = self.kept.get(line).copied().unwrap_or(0);
            counted += new.min(in_baseline);
            lost += new.min(in_baseline.saturating_sub(self.held.get(line).copied().unwrap_or(0)));
        }
        (counted, lost)
    }
}

/// The lines of `text`, without the empty one after its last newline.
fn lines_of(text: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = text.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// The blocks a member added to a file: each contiguous run of lines its
/// change from `before` to `after` writes, a run of blank lines left out. In
/// a release log (`release_log`) a run is split where a section begins (a
/// line starting `## `): one member's change can add several sections, and
/// a resolution can rightly put another member's section between them.
fn added_blocks<'a>(before: &'a [u8], after: &'a [u8], release_log: bool) -> Vec<Vec<&'a [u8]>> {
    use jj_lib::diff::{ContentDiff, DiffHunkKind};
    let mut blocks = Vec::new();
    for hunk in ContentDiff::by_line([before, after]).hunks() {
        let Some(added) = hunk
            .contents
            .get(1)
            .filter(|_| hunk.kind == DiffHunkKind::Different)
        else {
            continue;
        };
        let mut block: Vec<&[u8]> = Vec::new();
        for line in lines_of(added) {
            if release_log && line.starts_with(b"## ") && !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
            block.push(line);
        }
        blocks.push(block);
    }
    blocks.retain(|block| {
        block
            .iter()
            .any(|line| !line.iter().all(u8::is_ascii_whitespace))
    });
    blocks
}

/// Whether `text` holds `block`: a contiguous run of as many lines that
/// differs from it in at most one line, so a block whose heading was
/// renumbered is still there and one cut down or rewritten is not. A block
/// of fewer than three lines must appear exactly, or a heading alone would
/// hold a two-line section.
fn block_appears(block: &[&[u8]], text: &[u8]) -> bool {
    let tolerance = usize::from(block.len() >= 3);
    lines_of(text).windows(block.len()).any(|window| {
        window
            .iter()
            .zip(block)
            .filter(|(line, wanted)| line != wanted)
            .count()
            <= tolerance
    })
}

/// Whether the resolution dropped the member's change to a file: the
/// baseline held one of the blocks it added ([`added_blocks`]) and the
/// resolution holds none of them. A block an earlier recorded write already
/// dropped is not charged again.
fn blocks_dropped(
    (before, after): (&[u8], &[u8]),
    (baseline, resolution): (&[&[u8]], &[u8]),
    release_log: bool,
) -> bool {
    let blocks = added_blocks(before, after, release_log);
    let held = blocks
        .iter()
        .any(|block| baseline.iter().any(|side| block_appears(block, side)));
    held && !blocks.iter().any(|block| block_appears(block, resolution))
}

/// Adopt the commit `held` holds: abandon the earlier writes nothing rests on,
/// record the event, report. Every refusal comes first, so a refused adopt
/// writes nothing at all.
fn adopt(
    fork: &Fork<'_>,
    held: &Held,
    why: Option<&str>,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let ledger = Ledger::for_fork(fork).read()?.entries;
    let writes = EarlierWrites::read(&held.opened, &held.name, &ledger, held.published.as_ref())?;
    match held.standing(fork.entry, (&ledger, &writes))? {
        release::InHandWrite::NoRecord => {
            println!(
                "{repo}: the ledger records no cut or edit of {}, so there is no write for {} \
                 to replace; cut the release first, or, for a release cut outside knives, \
                 `knives release cut <name> --at <commit>`",
                held.shown_name(),
                held.commit.short()
            );
            return Ok(Exit::Incomplete);
        }
        release::InHandWrite::Unchecked(release::Unchecked::NoMatch(no_match)) => {
            let entry = fork.entry;
            let refusal =
                release::no_match_refusal(&held.in_hand(&ledger, &writes), &no_match, entry)?;
            println!("{repo}: {refusal}");
            return Ok(Exit::Incomplete);
        }
        release::InHandWrite::Recorded
        | release::InHandWrite::Unrecorded { .. }
        | release::InHandWrite::Unchecked(
            release::Unchecked::WriteAbsent(_) | release::Unchecked::Inherited { .. },
        ) => {}
    }
    if is_newest(repo, held, &ledger)? {
        return Ok(Exit::Ok);
    }
    let Some(reading) = held.reading(fork, (&ledger, &writes))? else {
        return Ok(Exit::Incomplete);
    };
    // The standing found the write that equals the commit or matches its
    // members, so there is one to replace.
    let Some(replaced) = Replaced::read(held, fork.entry, (&ledger, &writes), &reading.members)?
    else {
        return Ok(Exit::Incomplete);
    };
    let adopting = Adopting {
        fork,
        held,
        ledger: &ledger,
        replaced: &replaced,
    };
    if adopting.refuse_unfinished()? {
        return Ok(Exit::Incomplete);
    }
    let Some(adoption) = measure(&adopting, reading, why)? else {
        return Ok(Exit::Incomplete);
    };
    // Each earlier write that is now a visible head nothing rests on goes, as
    // after a cut: a resolution with its own change id leaves the write it
    // replaces as exactly such a head, which audit names as anonymous.
    let abandoned = knives::jj::abandon_unheld_writes(
        &fork.checkout.path,
        writes.commits(),
        Some(&held.commit),
        &format!(
            "knives: {}: abandon earlier writes nothing rests on",
            held.name
        ),
    )?;
    record(&adopting, &adoption, bound)?;
    report(&adopting, &adoption);
    report_abandoned_earlier(repo, &held.name, &abandoned.abandoned);
    report_hidden(repo, &held.name, &abandoned.hidden);
    Ok(if adoption.loses_a_member() {
        Exit::Findings
    } else {
        Exit::Ok
    })
}

/// Record the commit as the name's newest write: an edit event, which
/// `recorded_writes`, audit and every knives that reads edit events take
/// with no change. Its evidence is the commit, then each parent it lists
/// ([`Adoption::provenance`]). Its `parents` are a stacked release's previous
/// release, under its label, as every write of a stacked release records it;
/// then the replaced write's, copied with the branches they were recorded
/// under; then one per carrier, labelled. No reader takes a labelled parent
/// for a member. Its `fix_files` list the files of its fix by hand
/// ([`fix_files`]).
fn record(
    adopting: &Adopting<'_, '_>,
    adoption: &Adoption<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    let held = adopting.held;
    let mut parents: Vec<RecordedParent> = adoption
        .previous
        .iter()
        .map(|previous| RecordedParent {
            commit: previous.commit.as_str().to_owned(),
            branches: vec![previous_release_label(&previous.name)],
        })
        .collect();
    parents.extend(adopting.replaced.parents.iter().cloned());
    parents.extend(adoption.carriers.iter().map(|carrier| RecordedParent {
        commit: carrier.as_str().to_owned(),
        branches: vec![carrier_label(&held.name)],
    }));
    scribe_for(adopting.fork, bound)?.record(&Draft {
        evidence: std::iter::once(&held.commit)
            .chain(adoption.provenance.iter().map(|(_, commit)| commit))
            .map(|commit| commit.as_str().to_owned())
            .collect(),
        parents,
        fix_files: fix_files(&adoption.changes),
        ..Draft::event(
            Some(&held.name),
            edit_event_text(
                &held.name,
                (&held.commit, &adoption.change),
                &adoption.provenance,
                &format!("adopted: {}", adopted_text(adopting.replaced, adoption)),
            ),
            None,
        )
    })?;
    Ok(())
}

/// The adopt event's `fix_files`: the files `changes` resolved by hand,
/// then those it changed beyond the conflicts, each path whole. Its text
/// names them too, joined by `, `, as every knives reads them; a path that
/// holds a separator reads back whole only from this list.
fn fix_files(changes: &ResolutionChanges) -> Vec<FixFile> {
    let resolved = changes
        .resolved
        .iter()
        .map(|path| (path, FixChange::Resolved));
    let beyond = changes.beyond.iter().map(|path| (path, FixChange::Beyond));
    resolved
        .chain(beyond)
        .map(|(path, change)| FixFile {
            path: path.clone(),
            change,
        })
        .collect()
}

/// What the adopt event says changed, raw: the ledger keeps what the caller
/// and the repository said, and a reader escapes it.
fn adopted_text(replaced: &Replaced, adoption: &Adoption<'_>) -> String {
    let mut parts = vec![format!(
        "replaces {}; measured against {}",
        replaced.described(),
        adoption.baseline
    )];
    let changes = &adoption.changes;
    if !changes.resolved.is_empty() {
        parts.push(format!("{ADOPTED_RESOLVED}{}", changes.resolved.join(", ")));
    }
    match (changes.beyond.is_empty(), adoption.why) {
        (false, why) => parts.push(format!(
            "{ADOPTED_BEYOND}{} (why: {})",
            changes.beyond.join(", "),
            why.unwrap_or_default()
        )),
        (true, Some(why)) => parts.push(format!("why: {why}")),
        (true, None) => {}
    }
    if !adoption.carriers.is_empty() {
        let shorts: Vec<&str> = adoption.carriers.iter().map(CommitId::short).collect();
        parts.push(format!("carriers: {}", shorts.join(", ")));
    }
    if !adoption.lost.is_empty() {
        let counts: Vec<String> = adoption
            .lost
            .iter()
            .map(|lost| {
                format!(
                    "{} {} of {} in {}",
                    lost.member, lost.lost, lost.counted, lost.file
                )
            })
            .collect();
        parts.push(format!("added lines absent: {}", counts.join(", ")));
    }
    parts.join("; ")
}

/// Say what was adopted: the commit, the write it replaces and the baseline;
/// the files resolved by hand; the files changed beyond the conflicts, with
/// the reason; each member's lost lines; and each carrier, labelled.
fn report(adopting: &Adopting<'_, '_>, adoption: &Adoption<'_>) {
    let (repo, held) = (&adopting.fork.name, adopting.held);
    // Names, paths and labels are read from the repository, and the reason is
    // the caller's; each is escaped.
    println!(
        "{repo}: adopted {} as the newest write of {}, replacing {}",
        held.commit.short(),
        held.shown_name(),
        adopting.replaced.described()
    );
    println!("  baseline: {}", adoption.baseline);
    let changes = &adoption.changes;
    if changes.resolved.is_empty() {
        println!("  no file resolved by hand");
    } else {
        println!("  {} file(s) resolved by hand:", changes.resolved.len());
        for file in &changes.resolved {
            println!("    {}", inline_human_text(file));
        }
    }
    if changes.beyond.is_empty() {
        if let Some(why) = adoption.why {
            println!("  why: {}", inline_human_text(why));
        }
    } else {
        println!(
            "  {} file(s) changed beyond the baseline's conflicts, because: {}",
            changes.beyond.len(),
            inline_human_text(adoption.why.unwrap_or_default())
        );
        for file in &changes.beyond {
            println!("    {}", inline_human_text(file));
        }
    }
    for lost in &adoption.lost {
        let line = format!(
            "{}: {} of {} added line(s) of {} absent from the resolution",
            inline_human_text(&lost.member),
            lost.lost,
            lost.counted,
            inline_human_text(&lost.file)
        );
        // A loss that makes the exit findings is flagged, saying which.
        if !lost.is_finding(adoption.against_write) {
            println!("  {line}");
        } else if lost.lost == lost.counted {
            println!("  !! {line}: every line it added is gone");
        } else if lost.conflicted || knives::changelog_union::eligible(&lost.file) {
            println!("  !! {line}: none of the blocks it added is left");
        } else {
            println!("  !! {line}: a file the baseline left without a conflict");
        }
    }
    for carrier in &adoption.carriers {
        println!(
            "  carrier {}: {}",
            carrier.short(),
            inline_human_text(&carrier_label(&held.name))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{FileLines, LostLines, block_appears, blocks_dropped};

    /// One member's counts against one file's baseline and resolution
    /// ([`FileLines::counted_and_lost`]).
    fn counted_and_lost(
        (before, after): (&[u8], &[u8]),
        baseline: &[&[u8]],
        resolution: &[u8],
    ) -> (usize, usize) {
        FileLines::new(baseline, resolution).counted_and_lost((before, after))
    }

    #[test]
    fn a_line_the_resolution_holds_counts_as_held() {
        // Given: a member that adds one line to a file the baseline holds,
        // and a resolution that keeps it beside another member's.
        // When: the member's added lines are counted against the resolution.
        // Then: one counted, none absent.
        assert_eq!(
            counted_and_lost(
                (b"base\n", b"base\nalpha\n"),
                &[b"base\nalpha\n"],
                b"base\nalpha\nbeta\n"
            ),
            (1, 0)
        );
    }

    #[test]
    fn a_resolution_restored_from_the_base_holds_none_of_the_members_lines() {
        assert_eq!(
            counted_and_lost(
                (b"base\n", b"base\nbeta\nmore\n"),
                &[b"base\nbeta\nmore\n"],
                b"base\n"
            ),
            (2, 2)
        );
    }

    #[test]
    fn added_lines_count_as_a_multiset_and_blank_lines_not_at_all() {
        // Given: a member that adds one line twice with a blank line between,
        // a baseline holding both, and a resolution holding that line once.
        // When: its added lines are counted.
        // Then: the line was added twice and is absent once; the blank line
        // is no line at all.
        assert_eq!(
            counted_and_lost(
                (b"base\n", b"base\nx\n\nx\n"),
                &[b"base\nx\n\nx\n"],
                b"base\nx\n"
            ),
            (2, 1)
        );
    }

    #[test]
    fn a_line_the_baseline_already_lost_is_not_charged_again() {
        // Given: a member that added two lines, a baseline (an earlier
        // recorded write) that already holds only one, and a resolution that
        // holds neither.
        // When: its added lines are counted.
        // Then: only the line the baseline held is counted, and lost.
        assert_eq!(
            counted_and_lost(
                (b"base\n", b"base\nkept\ngone\n"),
                &[b"base\nkept\n"],
                b"base\n"
            ),
            (1, 1)
        );
    }

    #[test]
    fn a_conflicted_baseline_holds_each_sides_lines() {
        // Given: a baseline whose file is conflicted between the member's side
        // and another's, and a resolution keeping the other side only.
        // When: the member's added lines are counted.
        // Then: the member's line is held by the baseline through its side,
        // so it is counted and lost.
        assert_eq!(
            counted_and_lost(
                (b"base\n", b"base\nalpha\n"),
                &[b"base\nalpha\n", b"base\nbeta\n"],
                b"base\nbeta\n"
            ),
            (1, 1)
        );
    }

    #[test]
    fn a_line_the_baseline_holds_more_often_than_the_resolution_is_lost() {
        // Given: a member that added a line once, a baseline holding it twice
        // (another member added it too), and a resolution holding it once.
        // When: its added lines are counted.
        // Then: the line is lost once, capped at the member's own count.
        assert_eq!(
            counted_and_lost((b"base\n", b"base\nx\n"), &[b"base\nx\nx\n"], b"base\nx\n"),
            (1, 1)
        );
    }

    #[test]
    fn a_block_with_one_line_changed_still_appears_and_a_cut_down_one_does_not() {
        // Given: a member's three-line block.
        let block: [&[u8]; 3] = [b"## [1.1.0]", b"- alpha", b"- more"];
        // When/Then: renumbered, it appears; cut down to its heading, or with
        // two lines rewritten, it does not.
        assert!(block_appears(
            &block,
            b"top\n## [1.2.0]\n- alpha\n- more\nend\n"
        ));
        assert!(!block_appears(&block, b"top\n## [1.1.0]\nend\n"));
        assert!(!block_appears(
            &block,
            b"## [1.2.0]\n- alpha, reworded\n- more\n"
        ));
        // And: a block of fewer than three lines appears only exactly.
        assert!(block_appears(
            &[b"## [1.1.0]", b"- alpha"],
            b"x\n## [1.1.0]\n- alpha\n"
        ));
        assert!(!block_appears(
            &[b"## [1.1.0]", b"- alpha"],
            b"x\n## [1.2.0]\n- alpha\n"
        ));
        assert!(block_appears(&[b"- alpha"], b"x\n- alpha\n"));
        assert!(!block_appears(&[b"- alpha"], b"x\n- beta\n"));
    }

    #[test]
    fn a_member_is_dropped_only_when_the_baseline_held_a_block_the_resolution_lacks() {
        let (before, after): (&[u8], &[u8]) = (b"base\n", b"base\n## [1.1.0]\n- alpha\n- more\n");
        // Given/When/Then: a baseline holding the block, and a resolution
        // holding none of it: dropped.
        assert!(blocks_dropped(
            (before, after),
            (&[after], b"base\n"),
            false
        ));
        // And: the resolution renumbered it: not dropped.
        assert!(!blocks_dropped(
            (before, after),
            (&[after], b"base\n## [1.2.0]\n- alpha\n- more\n"),
            false
        ));
        // And: a baseline that already lacked it charges nothing again.
        assert!(!blocks_dropped(
            (before, after),
            (&[b"base\n"], b"base\n"),
            false
        ));
    }

    #[test]
    fn a_release_logs_sections_are_blocks_another_members_section_can_split() {
        // Given: a member whose one insert holds two sections, and a
        // resolution that puts another member's section between them.
        let before: &[u8] = b"# Log\n## [1.0.0]\n- base\n";
        let after: &[u8] = b"# Log\n## [1.2.0]\n- two\n## [1.1.0]\n- one\n## [1.0.0]\n- base\n";
        let resolution: &[u8] =
            b"# Log\n## [1.2.0]\n- two\n## [1.1.5]\n- other\n## [1.1.0]\n- one\n## [1.0.0]\n- base\n";
        // When/Then: read as a release log, each section is a block, so
        // neither is dropped; read as code, the one insert no longer appears.
        assert!(!blocks_dropped(
            (before, after),
            (&[after], resolution),
            true
        ));
        assert!(blocks_dropped(
            (before, after),
            (&[after], resolution),
            false
        ));
    }

    #[test]
    fn a_loss_is_a_finding_by_the_file_it_is_from_and_only_against_a_recorded_write() {
        // Given: one of a member's two counted lines lost, from a plain file,
        // a release log and a conflicted file, with or without its blocks.
        let lost = |file: &str, (conflicted, lost, dropped): (bool, usize, bool)| LostLines {
            member: "feat/alpha".to_owned(),
            file: file.to_owned(),
            counted: 2,
            lost,
            conflicted,
            dropped,
        };
        // When/Then: any lost line is a finding only in the plain file the
        // baseline left without a conflict; in a release log or a conflicted
        // file the finding is a member whose blocks are all gone.
        assert!(lost("src/lib.rs", (false, 1, false)).is_finding(true));
        assert!(!lost("src/lib.rs", (false, 0, false)).is_finding(true));
        assert!(!lost("CHANGELOG.md", (false, 1, false)).is_finding(true));
        assert!(lost("docs/RELEASE_NOTES.md", (false, 2, true)).is_finding(true));
        assert!(!lost("src/lib.rs", (true, 1, false)).is_finding(true));
        assert!(lost("src/lib.rs", (true, 2, true)).is_finding(true));
        // And: measured against an automerge, nothing is a finding.
        assert!(!lost("src/lib.rs", (false, 2, true)).is_finding(false));
    }
}
