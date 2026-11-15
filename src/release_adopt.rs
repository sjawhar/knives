//! `knives release adopt`: record a release finished by hand as its newest
//! knives write.
//!
//! A person who resolves a release's conflicts, renumbers a release log or
//! repairs the release commit leaves a commit knives did not write, and audit
//! reads it as drift from the write the ledger records. Adopting it is one
//! edit event, which every reader of knives' own writes already reads, after
//! the checks that make it that release finished: the recorded write's
//! parents plus any earlier write of the name a fast-forward push added, no
//! conflicts, and a reason for any change beyond the conflicts the write left.

use std::collections::BTreeMap;

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::commands::release;
use knives::forge::github::CliForge;
use knives::ids::{
    BookmarkRef, BranchName, ChangeId, CommitId, ReleaseScheme, RemoteName, RepoName,
};
use knives::jj::{Repo, ResolutionBaseline, ResolutionChanges};
use knives::ledger::{Draft, Entry, Ledger, RecordedParent, inline_human_text};
use knives::release_model::{
    RecordedWrite, carrier_label, carriers, members_event_text, recorded_write_events,
};

use super::release_edit::{
    published_refusal_text, recorded_parents, refuse_unresolved_trunk, report_abandoned_earlier,
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
    if !plan.problems.is_empty() {
        println!("{}", release::render(&plan));
        // The plan's render escaped these one line above; this summary
        // quotes the same jj, forge and consumer text.
        println!(
            "{repo}: adopt not applied; the plan could not answer: {}",
            plan.problems
                .iter()
                .map(|problem| knives::ledger::problem_human_text(problem))
                .collect::<Vec<_>>()
                .join("; ")
        );
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
    // Each member's lines are counted from its fork point with the trunk
    // (D8), the check that catches a resolution dropping a member's work, so
    // no adopt is recorded without it.
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
    parents: Vec<CommitId>,
    /// The publish remote's commit of the name, in jj's view.
    published: Option<CommitId>,
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
        let published = opened.bookmark_tips()?.remove(&BookmarkRef::Remote {
            branch: BranchName::new(name.as_str()),
            remote: RemoteName::new(remote),
        });
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
        }))
    }

    fn shown_name(&self) -> String {
        inline_human_text(&self.name)
    }
}

/// The write the adopted commit replaces: the newest recorded write of the
/// name.
struct Replaced {
    write: RecordedWrite,
    /// Whether this repository holds the write's commit. A write made on
    /// another machine and never pushed is not here.
    here: bool,
    /// The parent set its event records, each parent with every branch at it
    /// then, which `include` and `advance` match a member by once its
    /// bookmark has moved on.
    parents: Vec<RecordedParent>,
}

impl Replaced {
    /// The newest recorded write of `held`'s name, or `None` when the ledger
    /// records none.
    ///
    /// An event an older knives wrote has no `parents`; the set is then built
    /// as a write builds it, from the write's own parents where this
    /// repository holds it and from the event's evidence where it does not.
    fn read(
        held: &Held,
        entry: &knives::config::RepoEntry,
        ledger: &[Entry],
    ) -> anyhow::Result<Option<Self>> {
        let Some((event, write)) = recorded_write_events(ledger, &held.name).next() else {
            return Ok(None);
        };
        let here = held.opened.resolve_commit(write.commit.as_str()).is_ok();
        let parents = if event.parents.is_empty() {
            let commits = if here {
                held.opened.parent_commits(write.commit.as_str())?
            } else {
                event
                    .evidence
                    .iter()
                    .skip(1)
                    .map(|commit| CommitId::new(commit.as_str()))
                    .collect()
            };
            recorded_parents(&held.opened, entry, &commits)?
        } else {
            event.parents.clone()
        };
        Ok(Some(Self {
            write,
            here,
            parents,
        }))
    }

    fn commits(&self) -> Vec<CommitId> {
        self.parents
            .iter()
            .map(|parent| CommitId::new(parent.commit.as_str()))
            .collect()
    }

    /// `the recorded edit 0123456789ab`.
    fn described(&self) -> String {
        format!(
            "the recorded {} {}",
            self.write.kind,
            self.write.commit.short()
        )
    }

    /// Whether `held` already is this write, saying so: the write itself, or
    /// the write re-signed or metaedited, which audit already reads as it.
    fn is_held(&self, repo: &RepoName, held: &Held) -> anyhow::Result<bool> {
        let (shown, at) = (held.shown_name(), held.commit.short());
        if held.commit == self.write.commit {
            println!(
                "{repo}: {shown} is at {at}, {}; nothing to adopt",
                self.described()
            );
            return Ok(true);
        }
        if self.here
            && held
                .opened
                .is_metadata_rewrite(self.write.commit.as_str(), held.commit.as_str())?
        {
            println!(
                "{repo}: {shown} is at {at}, {} re-signed or metaedited (same change, parents \
                 and tree); nothing to adopt",
                self.described()
            );
            return Ok(true);
        }
        Ok(false)
    }
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
    /// The parents of the commit that are earlier writes of the name, or
    /// `None` after refusing a parent set that is not the replaced write's
    /// plus such carriers.
    ///
    /// The replaced write itself is no carrier here: a commit on top of it is
    /// a resolution made as a child of the release, which the release commit
    /// should hold instead.
    fn carriers_or_refuse(&self) -> anyhow::Result<Option<Vec<CommitId>>> {
        let (held, replaced) = (self.held, self.replaced);
        let recorded = replaced.commits();
        let extra: Vec<CommitId> = held
            .parents
            .iter()
            .filter(|parent| !recorded.contains(parent))
            .cloned()
            .collect();
        let found: Vec<CommitId> =
            carriers(&held.name, &extra, self.ledger, held.published.as_ref())
                .into_iter()
                .filter(|carrier| *carrier != replaced.write.commit)
                .collect();
        let child = held.parents.contains(&replaced.write.commit);
        let strays: Vec<CommitId> = extra
            .into_iter()
            .filter(|parent| !found.contains(parent) && *parent != replaced.write.commit)
            .collect();
        let missing: Vec<&RecordedParent> = replaced
            .parents
            .iter()
            .filter(|parent| {
                !held
                    .parents
                    .contains(&CommitId::new(parent.commit.as_str()))
            })
            .collect();
        if strays.is_empty() && missing.is_empty() && !child {
            return Ok(Some(found));
        }
        self.refuse_parent_set(&strays, &missing, child)?;
        Ok(None)
    }

    fn refuse_parent_set(
        &self,
        strays: &[CommitId],
        missing: &[&RecordedParent],
        child: bool,
    ) -> anyhow::Result<()> {
        let (repo, held, replaced) = (&self.fork.name, self.held, self.replaced);
        let (shown, at) = (held.shown_name(), held.commit.short());
        let entry = self.fork.entry;
        println!(
            "{repo}: {shown} at {at} does not have {}'s parents plus earlier writes of \
             {shown}, so it is not that release finished by hand:",
            replaced.described()
        );
        // Each source names a bookmark read from the repository.
        for (source, commit) in
            release::parent_sources(&held.opened, entry, &entry.release_scheme(), strays)?
        {
            println!(
                "  extra parent {} ({}): no parent of the recorded write, and no earlier write \
                 of {shown}",
                commit.short(),
                inline_human_text(&source)
            );
        }
        for parent in missing {
            let branches = if parent.branches.is_empty() {
                "no branch recorded".to_owned()
            } else {
                parent.branches.join(", ")
            };
            println!(
                "  missing parent {} ({}): a parent of the recorded write",
                CommitId::new(parent.commit.as_str()).short(),
                inline_human_text(&branches)
            );
        }
        if child {
            let release = replaced.write.commit.short();
            println!(
                "  {at} is a child of the release commit {release}: squash the resolution into \
                 the release commit (`jj squash --from {at} --into {release}`), then adopt again"
            );
        }
        Ok(())
    }

    /// Refuse, saying why, a commit that is not finished: one that still has
    /// conflicts, one a workspace has checked out, and one that would replace
    /// a published write the publish remote does not hold. `true` after
    /// refusing.
    fn refuse_unfinished(&self) -> anyhow::Result<bool> {
        let (repo, held) = (&self.fork.name, self.held);
        let (shown, at) = (held.shown_name(), held.commit.short());
        let conflicted =
            knives::jj::conflicted_files(&self.fork.checkout.path, held.commit.as_str())?;
        if !conflicted.is_empty() {
            println!(
                "{repo}: {shown} at {at} still has {} conflicted file(s), so it is not resolved \
                 yet; resolve them, then adopt:",
                conflicted.len()
            );
            // The paths are jj's, named by the conflict it reports.
            for file in &conflicted {
                println!("    {}", inline_human_text(file));
            }
            return Ok(true);
        }
        let change = held.opened.change_id_of(held.commit.as_str())?;
        let checked_out: Vec<String> = held
            .opened
            .workspaces()?
            .into_iter()
            .filter(|(_, working)| *working == change)
            .map(|(workspace, _)| inline_human_text(workspace.as_str()))
            .collect();
        if !checked_out.is_empty() {
            println!(
                "{repo}: {shown} at {at} is the working copy of workspace {}; run `jj new` there \
                 first, so editing in that workspace cannot change the adopted commit",
                checked_out.join(", ")
            );
            return Ok(true);
        }
        self.refuse_published()
    }

    /// Refuse adopting over a published write while the publish remote does
    /// not hold the commit: consumers were given the write, and adopting would
    /// record a release they were never given. `true` after refusing.
    ///
    /// A write this repository does not hold can carry no tag here, but a
    /// `record: published` note names it by its id all the same, so that note
    /// is read without the commit.
    fn refuse_published(&self) -> anyhow::Result<bool> {
        let (held, replaced) = (self.held, self.replaced);
        if held.published.as_ref() == Some(&held.commit) {
            return Ok(false);
        }
        let scheme = self.fork.entry.release_scheme();
        let evidence = if replaced.here {
            release::published_release(
                &held.opened,
                self.ledger,
                release::ReleaseAt {
                    scheme: &scheme,
                    name: &held.name,
                    commits: &[Some(&replaced.write.commit)],
                },
            )?
        } else if matches!(scheme, ReleaseScheme::Fixed(_)) {
            // A fixed branch moves forward by design: `published_release`
            // marks none of its positions published.
            None
        } else {
            release::published_note(self.ledger, &held.name, &[&replaced.write.commit])
        };
        let Some(evidence) = evidence else {
            return Ok(false);
        };
        println!(
            "{}: {}; {} does not hold {}",
            self.fork.name,
            published_refusal_text(&held.name, &evidence, ("adopting", "adopt")),
            self.fork.entry.publish_remote(),
            held.commit.short()
        );
        Ok(true)
    }
}

/// What adopting measured: everything the event and the report say.
struct Adoption<'a> {
    change: ChangeId,
    /// The parents that are earlier writes of the name.
    carriers: Vec<CommitId>,
    /// Each parent with the name a reader knows it by.
    provenance: Vec<(String, CommitId)>,
    /// What the commit was measured against, as a reader is told.
    baseline: String,
    changes: ResolutionChanges,
    why: Option<&'a str>,
    /// D8's counts, one per member and resolved file it added lines to.
    lost: Vec<LostLines>,
}

impl Adoption<'_> {
    /// A member every added line of whose file the resolution dropped: the
    /// loss a reviewer missed, which makes `adopt` exit with findings.
    fn loses_a_member(&self) -> bool {
        self.lost.iter().any(|lost| lost.absent == lost.added)
    }
}

/// Measure the commit against its baseline, or `None` after refusing a change
/// beyond the baseline's conflicts that has no reason.
fn measure<'a>(
    adopting: &Adopting<'_, '_>,
    carriers: Vec<CommitId>,
    why: Option<&'a str>,
) -> anyhow::Result<Option<Adoption<'a>>> {
    let (fork, held, replaced) = (adopting.fork, adopting.held, adopting.replaced);
    let entry = fork.entry;
    let members: Vec<CommitId> = held
        .parents
        .iter()
        .filter(|parent| !carriers.contains(parent))
        .cloned()
        .collect();
    let (baseline, described) = if replaced.here {
        (
            ResolutionBaseline::Written(&replaced.write.commit),
            replaced.described(),
        )
    } else {
        (
            ResolutionBaseline::Automerge(&members),
            format!(
                "an automerge of its {} member parent(s) with the release-log union, because \
                 this repository does not hold {}",
                members.len(),
                replaced.described()
            ),
        )
    };
    let changes = knives::jj::resolution_changes(&fork.checkout.path, baseline, &held.commit)?;
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
    let scheme = entry.release_scheme();
    let mut provenance = release::parent_sources(&held.opened, entry, &scheme, &held.parents)?;
    for (source, commit) in &mut provenance {
        if carriers.contains(commit) {
            *source = carrier_label(&held.name);
        }
    }
    // The trunk resolved before the release was read (`refuse_unresolved_trunk`).
    let trunk = held.opened.resolve_commit(&entry.trunk_ref())?;
    let member_sources: Vec<(String, CommitId)> = provenance
        .iter()
        .filter(|(_, commit)| members.contains(commit))
        .cloned()
        .collect();
    let lost = lost_lines(
        &held.opened,
        &trunk,
        &member_sources,
        (&held.commit, &changes.resolved),
    )?;
    Ok(Some(Adoption {
        change: held.opened.change_id_of(held.commit.as_str())?,
        carriers,
        provenance,
        baseline: described,
        changes,
        why,
        lost,
    }))
}

/// One member's added lines of one resolved file, never none, and how many of
/// them the resolution does not hold.
struct LostLines {
    member: String,
    file: String,
    added: usize,
    absent: usize,
}

/// D8's counts: for each resolved file and each member that added lines to
/// it, the member's added non-blank lines, from its fork point with the trunk
/// to its tip, that the resolution holds fewer times than the member added
/// them.
///
/// The fork point is the member's own, so a trunk that moved on since is never
/// charged to it. Counted as a multiset because a release log repeats lines
/// (`### Added`) that one member adding and another keeping must not cancel.
fn lost_lines(
    opened: &Repo,
    trunk: &CommitId,
    members: &[(String, CommitId)],
    (resolution, files): (&CommitId, &[String]),
) -> anyhow::Result<Vec<LostLines>> {
    let mut lost = Vec::new();
    for file in files {
        let resolved = opened.file_bytes(resolution, file)?.unwrap_or_default();
        for (member, tip) in members {
            let fork = opened
                .common_ancestor(std::slice::from_ref(tip), trunk)?
                .unwrap_or_else(|| trunk.clone());
            let before = opened.file_bytes(&fork, file)?.unwrap_or_default();
            let after = opened.file_bytes(tip, file)?.unwrap_or_default();
            let (added, absent) = added_and_absent(&before, &after, &resolved);
            if added > 0 {
                lost.push(LostLines {
                    member: member.clone(),
                    file: file.clone(),
                    added,
                    absent,
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

/// How many non-blank lines `after` adds to `before`, as a multiset, and how
/// many of those `resolution` holds fewer times than they were added.
fn added_and_absent(before: &[u8], after: &[u8], resolution: &[u8]) -> (usize, usize) {
    let (before, after, held) = (
        line_counts(before),
        line_counts(after),
        line_counts(resolution),
    );
    let mut added = 0;
    let mut absent = 0;
    for (line, count) in after {
        let new = count.saturating_sub(before.get(line).copied().unwrap_or(0));
        added += new;
        absent += new.saturating_sub(held.get(line).copied().unwrap_or(0));
    }
    (added, absent)
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
    let Some(replaced) = Replaced::read(held, fork.entry, &ledger)? else {
        println!(
            "{repo}: the ledger records no cut or edit of {}, so there is no write for {} to \
             replace; cut the release first",
            held.shown_name(),
            held.commit.short()
        );
        return Ok(Exit::Incomplete);
    };
    if replaced.is_held(repo, held)? {
        return Ok(Exit::Ok);
    }
    let adopting = Adopting {
        fork,
        held,
        ledger: &ledger,
        replaced: &replaced,
    };
    let Some(carriers) = adopting.carriers_or_refuse()? else {
        return Ok(Exit::Incomplete);
    };
    if adopting.refuse_unfinished()? {
        return Ok(Exit::Incomplete);
    }
    let Some(adoption) = measure(&adopting, carriers, why)? else {
        return Ok(Exit::Incomplete);
    };
    // Each earlier write that is now a visible head nothing rests on goes, as
    // after a cut: a resolution with its own change id leaves the write it
    // replaces as exactly such a head, which audit names as anonymous.
    let earlier = release::earlier_writes(&held.opened, &ledger, &held.name)?;
    let abandoned = knives::jj::abandon_unheld_writes(
        &fork.checkout.path,
        &earlier,
        &format!(
            "knives: {}: abandon earlier writes nothing rests on",
            held.name
        ),
    )?;
    record(&adopting, &adoption, bound)?;
    report(&adopting, &adoption);
    report_abandoned_earlier(repo, &held.name, &abandoned);
    Ok(if adoption.loses_a_member() {
        Exit::Findings
    } else {
        Exit::Ok
    })
}

/// Record the commit as the name's newest write: an edit event, which
/// `recorded_writes`, audit and every knives that reads edit events take
/// with no change. Its `parents` are the replaced write's, copied with the
/// branches they were recorded under, then one per carrier, labelled so no
/// reader takes it for a member.
fn record(
    adopting: &Adopting<'_, '_>,
    adoption: &Adoption<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    let held = adopting.held;
    let mut parents = adopting.replaced.parents.clone();
    parents.extend(adoption.carriers.iter().map(|carrier| RecordedParent {
        commit: carrier.as_str().to_owned(),
        branches: vec![carrier_label(&held.name)],
    }));
    scribe_for(adopting.fork, bound)?.record(&Draft {
        evidence: std::iter::once(&held.commit)
            .chain(&held.parents)
            .map(|commit| commit.as_str().to_owned())
            .collect(),
        parents,
        ..Draft::event(
            Some(&held.name),
            format!(
                "edited {} as {} (change {}) with {} parent(s): adopted: {}; parents: {}",
                held.name,
                held.commit.short(),
                adoption.change.short(),
                held.parents.len(),
                adopted_text(adopting.replaced, adoption),
                members_event_text(&adoption.provenance)
            ),
            None,
        )
    })?;
    Ok(())
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
        parts.push(format!("resolved by hand: {}", changes.resolved.join(", ")));
    }
    match (changes.beyond.is_empty(), adoption.why) {
        (false, why) => parts.push(format!(
            "changed beyond the baseline's conflicts: {} (why: {})",
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
                    lost.member, lost.absent, lost.added, lost.file
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
            lost.absent,
            lost.added,
            inline_human_text(&lost.file)
        );
        // A member every added line of which is gone is flagged.
        if lost.absent == lost.added {
            println!("  !! {line}: every line it added is gone");
        } else {
            println!("  {line}");
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
    use super::added_and_absent;

    #[test]
    fn a_line_the_resolution_holds_counts_as_held() {
        // Given: a member that adds one line to a file, and a resolution that
        // keeps it beside another member's.
        // When: the member's added lines are counted against the resolution.
        // Then: one added, none absent.
        assert_eq!(
            added_and_absent(b"base\n", b"base\nalpha\n", b"base\nalpha\nbeta\n"),
            (1, 0)
        );
    }

    #[test]
    fn a_resolution_restored_from_the_base_holds_none_of_the_members_lines() {
        assert_eq!(
            added_and_absent(b"base\n", b"base\nbeta\nmore\n", b"base\n"),
            (2, 2)
        );
    }

    #[test]
    fn added_lines_count_as_a_multiset_and_blank_lines_not_at_all() {
        // Given: a member that adds one line twice with a blank line between,
        // and a resolution holding that line once.
        // When: its added lines are counted.
        // Then: the line was added twice and is absent once; the blank line
        // is no line at all.
        assert_eq!(
            added_and_absent(b"base\n", b"base\nx\n\nx\n", b"base\nx\n"),
            (2, 1)
        );
    }
}
