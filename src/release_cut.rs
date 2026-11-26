//! `knives release` (plan and cut) and `knives release reap`.
//!
//! A cut is one merge of the carried members, gated on what the previous cut
//! recorded, what the working copies hold, and what would be orphaned; the
//! ledger event and the superseded-cut reap follow it. Rebasing the
//! composition lives in `release_rebase`; the membership verbs in
//! `release_edit`.

use std::cell::OnceCell;
use std::collections::BTreeMap;

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::commands::release;
use knives::config::Role;
use knives::forge::github::CliForge;
use knives::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, RepoName};
use knives::ledger::{Draft, Kind, Ledger, inline_human_text, problem_human_text, shell_word};
use knives::release_model::{
    CompositionBaseline, EarlierWrites, RecordedCut, RecordedWriteKind, StackedHistoryContext,
    carried_branches, members_event_text, previous_release_for_cut, previous_release_for_record,
    read_release, series_members, trunk_positions, trunk_positions_in, trunk_reaches,
};
use knives::store::{BranchLock, ForkLock};

use super::release_edit::{announce_unchecked, recorded_parents, report_hidden, union_event_text};
use super::scribe_for;

pub(crate) enum ReleaseInvocation {
    Plan,
    Cut {
        name: Option<String>,
        /// Each drop `--allow-drop` names ([`AllowDrop`]).
        allow_drop: Vec<String>,
        /// The commit `--at` records `name` at: a release cut outside knives,
        /// or on a fixed branch a commit its publish remote holds.
        at: Option<String>,
        /// The recorded write `--corrects` withdraws as the fixed branch's
        /// published commit is recorded in its place.
        corrects: Option<String>,
    },
}

/// The drops that `--allow-drop` names: each one a cut or a record may go
/// ahead without, and only those.
///
/// A value names a member, a commit of the orphan gate, or the newest
/// recorded write where this repository does not hold it, by its commit (at
/// least its first 12 characters), and a file of a fix by hand by its path,
/// for every adopt that the subject lacks there, or as `ADOPT:PATH` for that
/// adopt alone. A drop that no value names refuses the write, and so does a
/// value that names nothing the write drops ([`AllowDrop::settle`]). The
/// lines that state each drop are held until then, so a refusal is never
/// told after a line that reads as if the write went ahead.
pub(crate) struct AllowDrop {
    named: Vec<String>,
    used: std::cell::RefCell<Vec<bool>>,
    /// The lines that state what the write goes ahead without
    /// ([`Self::state`]), printed by [`Self::settle`].
    stated: std::cell::RefCell<Vec<String>>,
}

impl AllowDrop {
    pub(crate) fn new(named: Vec<String>) -> Self {
        let used = std::cell::RefCell::new(vec![false; named.len()]);
        Self {
            named,
            used,
            stated: std::cell::RefCell::default(),
        }
    }

    /// Whether a value names what `names` accepts; each such value counts
    /// as used.
    fn names(&self, names: impl Fn(&str) -> bool) -> bool {
        let mut found = false;
        for (value, used) in self.named.iter().zip(self.used.borrow_mut().iter_mut()) {
            if names(value) {
                *used = true;
                found = true;
            }
        }
        found
    }

    /// Whether a value names `commit`.
    fn commit(&self, commit: &CommitId) -> bool {
        self.names(|value| names_commit(value, commit))
    }

    /// Whether a value names the file `path` of the fix by hand of `adopt`.
    fn file(&self, adopt: &CommitId, path: &str) -> bool {
        self.names(|value| {
            value == path
                || value
                    .split_once(':')
                    .is_some_and(|(commit, file)| file == path && names_commit(commit, adopt))
        })
    }

    /// Whether a value names every drop of `check`, held against `held`:
    /// each member it lacks, each file of a fix by hand it lacks, and the
    /// newest write where this repository does not hold it. Every drop is
    /// asked, so each value that names one counts as used.
    fn names_every(&self, held: HeldRecords<'_>, check: &release::CompositionCheck) -> bool {
        let mut every = true;
        for member in &check.dropped {
            every &= self.commit(&member.commit);
        }
        for fix in &check.dropped_fixes {
            for file in &fix.files {
                every &= self.file(&fix.commit, &file.path);
            }
        }
        if let Some(write) = held.newest_write().filter(|_| check.absent_write) {
            every &= self.commit(&write.recorded().commit);
        }
        every
    }

    /// Hold `lines`, which state what the write goes ahead without, for
    /// [`Self::settle`] to print.
    fn state(&self, lines: impl IntoIterator<Item = String>) {
        self.stated.borrow_mut().extend(lines);
    }

    /// After a write has asked every gate: the refusal of a write where a
    /// value names nothing that the write drops, exit 3, nothing written and
    /// no stated drop printed; otherwise each stated line ([`Self::state`]),
    /// printed now, and `None`.
    fn settle(&self, repo: &RepoName) -> Option<Exit> {
        let unused: Vec<String> = self
            .named
            .iter()
            .zip(self.used.borrow().iter())
            .filter(|(_, used)| !**used)
            .map(|(value, _)| shell_word(value))
            .collect();
        if unused.is_empty() {
            for line in self.stated.borrow_mut().drain(..) {
                println!("{line}");
            }
            return None;
        }
        println!(
            "{repo}: refusing: --allow-drop {} names no drop of this write; nothing was written",
            unused.join(", ")
        );
        Some(Exit::Incomplete)
    }
}

/// Whether `value` names `commit`: one begins with the other, and `value`
/// gives at least as much of it as its short form does.
fn names_commit(value: &str, commit: &CommitId) -> bool {
    let commit = commit.as_str();
    !value.is_empty()
        && !commit.is_empty()
        && value.len() >= commit.len().min(12)
        && (commit.starts_with(value) || value.starts_with(commit))
}

/// The line that gives the `--allow-drop` of each drop that `drops` names
/// ([`drop_names`]), each quoted for the shell: `  name each drop:
/// --allow-drop X --allow-drop Y`.
fn drop_flags_line(drops: &[String]) -> String {
    let flags: Vec<String> = drops
        .iter()
        .map(|drop| format!("--allow-drop {}", shell_word(drop)))
        .collect();
    format!("  name each drop: {}", flags.join(" "))
}

/// The name of each drop of `check`, held against `held`, as `--allow-drop`
/// takes it ([`AllowDrop`]): each member's commit, short, each file of a
/// fix by hand once, by its path, then the newest write's commit, short,
/// where this repository does not hold it.
fn drop_names(held: HeldRecords<'_>, check: &release::CompositionCheck) -> Vec<String> {
    let mut names: Vec<String> = check
        .dropped
        .iter()
        .map(|member| member.commit.short().to_owned())
        .collect();
    for file in check.dropped_fixes.iter().flat_map(|fix| &fix.files) {
        if !names.contains(&file.path) {
            names.push(file.path.clone());
        }
    }
    if let Some(write) = held.newest_write().filter(|_| check.absent_write) {
        names.push(write.recorded().commit.short().to_owned());
    }
    names
}

#[allow(
    clippy::too_many_lines,
    reason = "cutting is one ordered transaction whose candidate, audits, composition gate, publication, and ledger event must stay together"
)]
pub(crate) fn run_release(
    fork: &Fork<'_>,
    extra_consumers: &[&std::path::Path],
    invocation: &ReleaseInvocation,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let scheme = entry.release_scheme();
    let RequestedCut {
        name: cut_name,
        allow_drop,
        at,
        corrects,
    } = match requested_cut(invocation, &scheme, entry) {
        Ok(request) => request,
        Err(exit) => return Ok(exit),
    };
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals: &locals,
        forge: &forge,
        cache_root: cache_root.as_deref(),
        heads: &heads,
    };
    let fetched = release::fetch(fork, bound.registry, &consumers);
    // A cut writes, so it takes its fork's lock here, once the network has
    // answered, and reads the repository and the ledger it decides from
    // under it ([`release::Fetched::plan`]); a plan writes nothing.
    let lock = CutLock::of(
        cut_name
            .is_some()
            .then(|| ForkLock::acquire(&fork.upstream))
            .transpose()?,
    );
    let opened = knives::jj::Repo::open(path)?;
    let (mut worst, pins) = release_plan_exit(fork, fetched, &consumers, &opened)?;
    if worst == Exit::Incomplete {
        return Ok(worst);
    }

    if let Some(name) = cut_name {
        let fixed_branch = matches!(scheme, ReleaseScheme::Fixed(_));
        // A release cut outside knives is recorded, never composed: no orphan
        // gate, no candidate, no reap.
        if let Some(at) = at
            && !fixed_branch
        {
            return Ok(worst.worst(record_hand_cut(
                (fork, &opened, &lock),
                (&name, at, &allow_drop),
                bound,
            )?));
        }
        let trunk_name = entry.trunk_ref();
        let trunk = opened.resolve_commit(&trunk_name)?;
        // A record moves nothing, so `--at` strands nothing.
        let orphaned = match at {
            Some(_) => None,
            None => check_orphan_commits_before_cut(&opened, fork)?,
        };
        if let Some(orphaned) = &orphaned
            && let Some(exit) = report_orphaned_cut(repo, orphaned, &allow_drop)
        {
            return Ok(exit);
        }
        let tips = opened.bookmark_tips()?;
        // A fixed branch's `--at REV` records REV, a commit its publish
        // remote holds, as the composition in hand: the block below records
        // it as it records the publish remote's own commit.
        let previous = match at {
            Some(at) => match fixed_record_at(repo, (&opened, entry, &tips), (&name, at))? {
                Ok(held) => Some(held),
                Err(exit) => return Ok(exit),
            },
            None => previous_release_for_cut(entry, &tips),
        };
        let previous_commit = previous.as_ref().map(|(_, commit)| commit.clone());
        // A dated cut takes a name that sorts after the newest release in the
        // series' one order: the reap that follows every dated cut keeps only
        // the newest name, so any other name would be created and taken
        // straight back as superseded in the same command, reported as a
        // success that left nothing behind.
        if let Some((previous_ref, _)) = &previous
            && matches!(scheme, ReleaseScheme::Dated | ReleaseScheme::Series(_))
            && scheme.cut_key(&name) <= scheme.cut_key(previous_ref.branch().as_str())
        {
            println!(
                "{repo}: refusing to cut {name}: {} is the newest release, so a dated cut takes a name that sorts after it (the reap after a cut keeps only the newest name)",
                inline_human_text(previous_ref.branch().as_str())
            );
            return Ok(Exit::Incomplete);
        }
        // The pulled plan refused on any problem this read finds.
        let mut ledger = Ledger::for_fork(fork).read()?.entries;
        // `--corrects` withdraws a recorded write the publish remote never
        // held. Every read below reads the ledger with the withdrawal in it;
        // the withdrawal is written only with the record that follows it.
        let withdrawal = match corrects {
            Some(corrects) => match correction(
                (repo, &opened, entry, &tips),
                (&name, corrects, at.is_some()),
                (&ledger, previous.as_ref()),
            )? {
                Ok(withdrawal) => {
                    ledger.push(withdrawal.entry());
                    Some(withdrawal)
                }
                Err(exit) => return Ok(exit),
            },
            None => None,
        };
        // A cut is a new name for the composition in hand, never a recomputation:
        // with a previous release its parents are carried verbatim — nothing joins,
        // nothing advances, and a branch enters through `release include`. Only the
        // first cut has no composition to carry, so it starts from every branch: a
        // release is a flat merge of feature and fix branches, and the upstream
        // base is never a direct parent — it is reachable through every member.
        // An earlier write of the previous release a fast-forward push gave it
        // is no member, so the cut carries every parent but those.
        let previous_published = previous.as_ref().and_then(|(reference, _)| {
            tips.get(&BookmarkRef::Remote {
                branch: reference.branch().clone(),
                remote: RemoteName::new(entry.publish_remote()),
            })
        });
        let previous_writes = match &previous {
            Some((reference, _)) => EarlierWrites::read(
                &opened,
                reference.branch().as_str(),
                &ledger,
                previous_published,
            )?,
            None => EarlierWrites::default(),
        };
        let (previous_carriers, previous_members) = match &previous {
            Some((_, commit)) => {
                previous_writes.partition(&opened.parent_commits(commit.as_str())?)
            }
            None => (Vec::new(), Vec::new()),
        };
        // A cut built over a release that carries a change knives did not
        // write or adopt would carry it forward under a new name, after which
        // it reads as recorded; so the tree decides before any audit does,
        // and the cut refuses whatever the audit's rows would say. The rows
        // name a release that is recorded as one; one knives could not check
        // is cut, says so, and its event carries the mark; so is one the
        // ledger records no write of, and its rows say nothing measured it.
        let (carried_from, unchecked) = match &previous {
            Some((reference, commit)) => {
                let in_hand = release::InHand {
                    repo: &opened,
                    ledger: &ledger,
                    name: reference.branch().as_str(),
                    commit,
                    writes: &previous_writes,
                    published: previous_published,
                };
                let standing = release::in_hand_write(&in_hand)?;
                if let Some(refusal) = release::in_hand_refusal(&in_hand, &standing, entry)? {
                    println!("{repo}: refusing to cut {name}: {refusal}");
                    println!("{repo}: cut discarded; nothing was written at all.");
                    return Ok(Exit::Incomplete);
                }
                let from = match &standing {
                    release::InHandWrite::Recorded => release::CarriedFrom::Recorded,
                    release::InHandWrite::Unchecked(_) => release::CarriedFrom::Unchecked,
                    release::InHandWrite::NoRecord => release::CarriedFrom::Unledgered,
                    // `in_hand_refusal` refuses every unrecorded release, so
                    // the refusal above has returned before this match.
                    release::InHandWrite::Unrecorded { .. } => anyhow::bail!(
                        "{repo}: the in-hand refusal returns first for an unrecorded release, \
                         yet the cut of {name} reached its carried rows over one; this is a bug"
                    ),
                };
                (from, announce_unchecked(repo, &in_hand, &standing))
            }
            None => (release::CarriedFrom::Unledgered, String::new()),
        };
        let (carried, audit_base) = if let Some((_, previous)) = &previous {
            let carried = release::parent_sources(&opened, entry, &scheme, &previous_members)?;
            let base =
                release::shared_base(&opened, previous, &trunk)?.unwrap_or_else(|| trunk.clone());
            (carried, base)
        } else {
            // The first cut of a series composes only that series' branches:
            // a branch on another series' base would carry that base in.
            let (carried, unfit) = series_members(
                &opened,
                entry,
                carried_branches(&opened, entry.trunk(), &scheme)?,
            )?;
            if !unfit.is_empty() {
                // Each line names a bookmark read from the repository.
                for line in &unfit {
                    println!("{repo}: {}", inline_human_text(line));
                }
                println!(
                    "{repo}: refusing the first cut of the {}: rebase each branch above onto \
                     one series' base, or remove its bookmark",
                    entry.series_label()
                );
                return Ok(Exit::Incomplete);
            }
            if carried.is_empty() {
                println!(
                    "{repo}: no branches to cut; a release is a flat merge of feature \
                     and fix branches, and the {} has none",
                    entry.series_label()
                );
                return Ok(Exit::Incomplete);
            }
            // The first cut composes every branch with no include to gate it,
            // so the gate include applies runs here: a branch whose history
            // carries a merge would make the cut carry everything that merge
            // carried, and the plan would report it the moment it existed.
            if refuse_stacked_first_cut(repo, &opened, entry, &carried)? {
                return Ok(Exit::Incomplete);
            }
            // The first cut's base is its members' fork point with the
            // trunk, not the trunk tip, which has every commit upstream
            // landed since the fork; the audit reads each member from its
            // own fork point.
            let member_tips: Vec<knives::ids::CommitId> =
                carried.iter().map(|(_, tip)| tip.clone()).collect();
            let base = opened.fork_point_of(&member_tips, &trunk)?;
            (carried, base)
        };
        let request = release::Cut::from_carried(name.clone(), &carried);
        let mut candidate = release::candidate_cut(
            path,
            &request,
            previous_commit
                .as_ref()
                .map(|previous| (previous, previous_carriers.as_slice())),
        )?;
        // The orphan gate protects unreachable commits; this protects the
        // composition itself. The ledger's events are the only record of a
        // parent set that survives the bookmark moving, so every subject is
        // held against the previous cut's event and the newest recorded write
        // of the newest release the ledger records before anything is
        // recorded or published. Each fix by hand that the series holds, an
        // `adopt` of any of its releases, is held file by file, each file
        // against its reference, the newest recorded write that holds it with
        // no conflict, where that reference still holds the fix, by the
        // candidate's own tree.
        // What this repository cannot read is never held to less: a newest
        // write it lacks refuses the cut, and a fix it cannot measure is
        // held, at each file that its adopt event can name, to what that
        // file's reference holds.
        let (baseline, fixes) =
            CompositionGate::read_baseline((&opened, entry), (&ledger, &tips), None)?;
        let gate = CompositionGate {
            baseline,
            record: None,
            opened: &opened,
            entry,
            lock: &lock,
            ledger: &ledger,
            path,
            parents: &request.parents,
            base: &audit_base,
            trunk: &trunk,
            tips: &tips,
            fixes,
        };
        // A cut is audited against the release it duplicates: a member's
        // lines that release already lacked are carried forward, and the
        // drift is measured from it. Each member is read from its own fork
        // point with the upstream trunk. A release in hand that no knives
        // write or adopt measured is no record of what its members hold, so
        // where the newest recorded write is another commit, its members are
        // read against that write instead, as a fixed branch's published
        // commit is: a line of a member that write did not record, or did not
        // already lack, is missing. That reading never checks less than one
        // against the release in hand itself, which lacks every line it lacks.
        let followed_in_hand = matches!(carried_from, release::CarriedFrom::Unchecked)
            .then(|| {
                gate.baseline.newest_write.as_ref().filter(|write| {
                    previous_commit.as_ref() != Some(&write.commit)
                        && opened.resolve_commit(write.commit.as_str()).is_ok()
                })
            })
            .flatten();
        let carried_from = if followed_in_hand.is_some() {
            release::CarriedFrom::NewestWrite
        } else {
            carried_from
        };
        let context = release::AuditContext {
            previous: followed_in_hand
                .map(|write| &write.commit)
                .or(previous_commit.as_ref()),
            forward_members: followed_in_hand.map(|write| write.members.as_slice()),
            upstream_trunk: &trunk,
            trunk_parents: &[],
            trunk_parent_base: None,
            drift_trunk: &audit_base,
            drift_from: previous_commit
                .as_ref()
                .map(|previous| (previous, release::DriftBase::PreviousRelease)),
        };
        // A cut names a composition consumers can pin. When the publish remote
        // already holds the previous cut with exactly this tree on exactly these
        // parents, a new name ships nothing and only burns a dated name and a
        // re-pin nobody asked for. The comparison is against the published copy:
        // the candidate is a duplicate of the in-hand previous release, so their
        // trees and parents always match locally.
        //
        // Two exceptions: a previous release every consumer pins by revision,
        // and a published one (a tag or a `record:` note names its commit).
        // `include`, `drop`, `advance` and `rebase` all refuse to edit either —
        // the edit would reach nobody, or move what was published — and send
        // the operator here. A verbatim cut under a new dated name is then the
        // only editable composition, so it is allowed and says why; the edits
        // and the re-pin follow it. The name has to be genuinely new: a fixed
        // branch has no other name to take (a dated name that does not sort
        // after the previous cut was refused above).
        //
        // A fixed branch has one name, so its cut records what is published.
        // Where the composition in hand is the publish remote's commit, a cut
        // would make that commit's tree on its parents, its earlier writes left
        // out (the candidate is the composition in hand, its carriers left
        // out), so the cut records that commit and never builds another.
        // `--at` names the commit it records, one the publish remote holds.
        if let Some((previous_ref, previous_tip)) = &previous {
            let publish_remote = entry.publish_remote();
            let remote_tip = tips.get(&BookmarkRef::Remote {
                branch: previous_ref.branch().clone(),
                remote: RemoteName::new(publish_remote),
            });
            let published = if at.is_some() {
                Some(previous_tip)
            } else {
                remote_tip
            };
            // The previous release is a name read from the repository, and
            // the evidence names a tag on it.
            let shown_previous = inline_human_text(previous_ref.branch().as_str());
            if let Some(published) = published
                && fixed_branch
                && (published == previous_tip || candidate.matches(published.as_str())?)
            {
                if gate
                    .baseline
                    .previous_cut
                    .as_ref()
                    .is_some_and(|recorded| recorded.commit == *published)
                {
                    println!(
                        "{repo}: refusing to cut {name}: identical to {shown_previous}@{publish_remote} ({}); nothing to cut",
                        published.short()
                    );
                    return Ok(Exit::Incomplete);
                }
                // The newest recorded write this repository holds: the
                // release the published commit follows.
                let followed = gate
                    .baseline
                    .newest_write
                    .as_ref()
                    .filter(|write| opened.resolve_commit(write.commit.as_str()).is_ok());
                if at.is_some()
                    && let Some(newest) = followed
                    && opened.is_ancestor(published, &newest.commit)?
                {
                    println!(
                        "{repo}: refusing to cut {name} --at {}: it is {}'s newest recorded write \
                         ({}) or in its history, so the ledger records a newer write of it; \
                         nothing was written",
                        published.short(),
                        inline_human_text(&name),
                        newest.commit.short()
                    );
                    return Ok(Exit::Incomplete);
                }
                // An abandoned write the publish remote never held is one
                // that `--corrects` withdraws; a visible one is still to be
                // pushed, and the gate below says to move the release to it.
                let remote_commit = remote_tip.unwrap_or(published);
                if let Some(unpushed) =
                    unpushed_newest_write(&opened, (&ledger, &name), remote_commit)?
                    && !opened.is_visible(&unpushed.commit)?
                {
                    let at = if at.is_some() {
                        format!(" --at {}", published.short())
                    } else {
                        String::new()
                    };
                    println!(
                        "{repo}: refusing to cut {name}: its newest write, {}, is abandoned and \
                         not in {shown_previous}@{publish_remote} ({})'s history, so the publish \
                         remote never held it; `knives release cut --corrects {}{at}` withdraws \
                         it and records {} in its place; nothing was written",
                        inline_human_text(&release::described_write(&unpushed)),
                        remote_commit.short(),
                        shell_word(unpushed.commit.short()),
                        published.short()
                    );
                    return Ok(Exit::Incomplete);
                }
                let parents = opened.parent_commits(published.as_str())?;
                let carriers = previous_writes.carriers(&parents);
                if carriers.is_empty()
                    && refuse_unknown_release_parents(
                        (repo, &opened),
                        (&name, published),
                        (&parents, &previous_writes),
                    )?
                {
                    return Ok(Exit::Incomplete);
                }
                // The published commit is read against the recorded write it
                // follows, never against itself: a line of a member that it
                // lacks is carried forward only where that write recorded the
                // member and lacked the line too. A member that write does not
                // record, or a write this repository does not hold, carries
                // nothing forward, as `cut --at` carries nothing.
                let committed = CommittedCut {
                    commit: published,
                    members: &carried,
                    context: release::AuditContext {
                        previous: followed.map(|write| &write.commit),
                        forward_members: Some(
                            followed.map_or(&[][..], |write| write.members.as_slice()),
                        ),
                        // The drift is the published commit's own change
                        // over the release it was made on, never over
                        // itself: a file no member explains is named.
                        drift_from: carriers
                            .first()
                            .or_else(|| followed.map(|write| &write.commit))
                            .map(|from| (from, release::DriftBase::PreviousRelease)),
                        ..context
                    },
                    carried_from: release::CarriedFrom::NewestWrite,
                    waivable: true,
                };
                let checked = match checked_committed_cut(repo, &committed, (&gate, &allow_drop))? {
                    Ok(checked) => checked,
                    Err(exit) => return Ok(exit),
                };
                if let Some(exit) = allow_drop.settle(repo) {
                    return Ok(exit);
                }
                let (audit, check) = (&checked.audit, &checked.check);
                let through_carriers = held_through_carriers(
                    &opened,
                    published,
                    followed.map_or(&[][..], |write| write.members.as_slice()),
                )?;
                let completed = CompletedCut {
                    name: &name,
                    request: &request,
                    created: published,
                    carriers: &carriers,
                    audit,
                    scheme: &scheme,
                    held: gate.held(),
                    check,
                    stated_drop: None,
                    waived: &checked.waived,
                    through_carriers: &through_carriers,
                    // Recorded as published, not written: knives unioned nothing.
                    unions: &[],
                    unchecked: &unchecked,
                    composition: None,
                };
                // The withdrawal's line names each member of the withdrawn
                // write that the published commit does not carry.
                if let Some(withdrawal) = &withdrawal {
                    let drops = withdrawal.drops(&gate)?;
                    withdrawal.record((fork, bound), &drops)?;
                }
                record_cut_event(fork, &completed, bound)?;
                println!(
                    "{}",
                    recorded_published_line(
                        repo,
                        (&name, publish_remote),
                        published,
                        gate.baseline.previous_cut.as_ref()
                    )
                );
                release::report_repeated_headings(path, published);
                if let Some(orphaned) = &orphaned {
                    report_stated_drop(fork, orphaned)?;
                }
                worst = worst.worst(completed.exit());
                return Ok(worst);
            }
            if let Some(published) = published
                && !fixed_branch
                && candidate.matches(published.as_str())?
            {
                let why = verbatim_cut_reason(
                    &opened,
                    (&pins, &scheme, &ledger),
                    (previous_ref, previous_tip, Some(published)),
                )?;
                let Some(why) = why else {
                    println!(
                        "{repo}: refusing to cut {name}: identical to {shown_previous}@{publish_remote} ({}); nothing to cut",
                        published.short()
                    );
                    return Ok(Exit::Incomplete);
                };
                println!(
                    "{repo}: {name} starts identical to {shown_previous}@{publish_remote} ({}); {why}, so this new name is the composition to edit — `include`, `advance`, `drop` or `rebase` it before pushing",
                    published.short()
                );
            }
        }
        // `--corrects` and a fixed branch's `--at` read only a composition in
        // hand that the publish remote holds ([`correction`],
        // [`fixed_record_at`]), which the block above records or refuses.
        anyhow::ensure!(
            withdrawal.is_none() && at.is_none(),
            "{repo}: --corrects or --at reached the composition of a new cut of {name}; this is a \
             bug"
        );
        // An audit error or failure simply DROPS the candidate: the merge
        // was never a published operation, so there is nothing to abandon
        // and no crash window that strands one. A file of a fix by hand that
        // the gate holds is the gate's to judge, not the drift check's.
        let mut audit = release::audit_cut(
            path,
            &carried,
            release::CutSubject::Candidate(&mut candidate),
            context,
        )?;
        audit.leave_fixes_to_gate(&gate.fixes);
        if let Some(exit) = report_cut_audit(repo, &audit, (carried_from, context.drift_from), &[])
        {
            return Ok(exit);
        }
        let check = {
            let mut subject = release::CutSubject::Candidate(&mut candidate);
            match recorded_composition_check(repo, &mut subject, &gate, &allow_drop)? {
                Ok(check) => check,
                Err(exit) => return Ok(exit),
            }
        };
        if let Some(exit) = allow_drop.settle(repo) {
            return Ok(exit);
        }
        // The publish rebuilds the audited tree, so the candidate's unions are
        // what ships.
        let unions = candidate.unions().to_vec();
        // The publish is the cut's write, read as soon as it is done: its
        // repeated headings print with its report, or once after a step
        // before the report that fails.
        worst = release::with_last_write_headings(|last| {
            let published = release::publish_cut(
                candidate,
                &request.name,
                &scheme,
                (previous_writes.commits(), Some(entry.publish_remote())),
            )?;
            let created = published.commit;
            *last = Some(release::ReportedWrite::read(path, created.clone()));
            let completed = CompletedCut {
                name: &name,
                request: &request,
                created: &created,
                carriers: &[],
                audit: &audit,
                scheme: &scheme,
                held: gate.held(),
                check: &check,
                stated_drop: orphaned.as_ref(),
                waived: &[],
                through_carriers: &[],
                unions: &unions,
                unchecked: &unchecked,
                composition: None,
            };
            record_cut_event(fork, &completed, bound)?;
            let exit = worst.worst(report_completed_cut(
                fork,
                &opened,
                &completed,
                (bound, last),
            )?);
            report_hidden(
                repo,
                previous
                    .as_ref()
                    .map_or(name.as_str(), |(reference, _)| reference.branch().as_str()),
                &published.hidden,
            );
            Ok(exit)
        })?;
    }
    Ok(worst)
}

/// What a release invocation asks a cut to do ([`requested_cut`]).
struct RequestedCut<'a> {
    /// The name a cut takes; `None` for a plan.
    name: Option<String>,
    allow_drop: AllowDrop,
    /// The commit `--at` records the name at.
    at: Option<&'a str>,
    /// The recorded write `--corrects` withdraws.
    corrects: Option<&'a str>,
}

/// The name a cut takes, `--allow-drop`, the commit `--at` records the name
/// at, and the write `--corrects` withdraws, or the usage error's exit after
/// saying why.
fn requested_cut<'a>(
    invocation: &'a ReleaseInvocation,
    scheme: &knives::ids::ReleaseScheme,
    entry: &knives::config::RepoEntry,
) -> Result<RequestedCut<'a>, Exit> {
    match invocation {
        ReleaseInvocation::Plan => Ok(RequestedCut {
            name: None,
            allow_drop: AllowDrop::new(Vec::new()),
            at: None,
            corrects: None,
        }),
        ReleaseInvocation::Cut {
            name,
            allow_drop,
            at,
            corrects,
        } => {
            if corrects.is_some() && !matches!(scheme, ReleaseScheme::Fixed(_)) {
                eprintln!(
                    "--corrects withdraws a recorded write of a fixed release branch as a plain \
                     `knives release cut` records its published commit, and this repo has no \
                     fixed release branch"
                );
                return Err(Exit::Usage);
            }
            // A series' own prefix is refused first, with its own reason: the
            // grammar would refuse it too, as a name with no date.
            match name
                .as_deref()
                .map_or(Ok(()), |name| {
                    release::clear_of_series_prefixes(entry, name)
                })
                .and_then(|()| release::cut_name(scheme, name.as_deref()))
            {
                Ok(name) => Ok(RequestedCut {
                    name: Some(name),
                    allow_drop: AllowDrop::new(allow_drop.clone()),
                    at: at.as_deref(),
                    corrects: corrects.as_deref(),
                }),
                Err(message) => {
                    eprintln!("{message}");
                    Err(Exit::Usage)
                }
            }
        }
    }
}

/// The withdrawal of a recorded write of a fixed release branch that the
/// publish remote never held, which `cut --corrects` writes just before the
/// event that records the branch's published commit in its place
/// ([`knives::release_model::Withdrawn`]).
struct Withdrawal {
    /// The fixed branch.
    name: String,
    kind: RecordedWriteKind,
    /// The withdrawn write.
    write: CommitId,
    /// The members its event records
    /// ([`knives::release_model::recorded_write_members`]).
    members: Vec<CommitId>,
    /// The publish remote and its commit of the branch.
    remote: String,
    published: CommitId,
}

impl Withdrawal {
    /// The event's text as the reads before the record see it, which the
    /// recorded text begins with ([`Self::recorded_text`]).
    fn text(&self) -> String {
        knives::release_model::withdrawal_text(
            &self.name,
            (self.kind, &self.write),
            (&self.remote, &self.published),
        )
    }

    /// The event's text as it is recorded and printed: [`Self::text`], then
    /// each member of the withdrawn write that the published commit does not
    /// carry, of `drops` ([`Self::drops`]), or that it carries every one.
    /// The names are raw.
    fn recorded_text(&self, drops: &[release::RecordedMember]) -> String {
        let published = self.published.short();
        if drops.is_empty() {
            return format!("{}; {published} carries every member of it", self.text());
        }
        let names: Vec<&str> = drops.iter().map(|member| member.name.as_str()).collect();
        format!(
            "{}; {published} does not carry {} member(s) of it: {}",
            self.text(),
            names.len(),
            names.join(", ")
        )
    }

    fn evidence(&self) -> Vec<String> {
        vec![
            self.write.as_str().to_owned(),
            self.published.as_str().to_owned(),
        ]
    }

    /// The event as the ledger will hold it, for the reads before it is
    /// written: its subject and evidence are what [`Self::record`] writes,
    /// and its text is what the recorded text begins with, all that a read
    /// takes from it ([`knives::release_model::withdrawn_write`]).
    fn entry(&self) -> knives::ledger::Entry {
        knives::ledger::Entry {
            ts: String::new(),
            owner: String::new(),
            email: None,
            subject: Some(self.name.clone()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: self.text(),
            evidence: self.evidence(),
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

    /// Each member of the withdrawn write that the published commit does not
    /// carry: the members of that write alone, each held against the
    /// published commit as the composition gate holds a recorded write's
    /// ([`release::uncarried_recorded_members`]), with the gate's parents,
    /// base, trunk and tips. The gate itself holds the published commit
    /// against the writes the withdrawal leaves, so only this names what
    /// leaves the record with the withdrawn write.
    fn drops(&self, gate: &CompositionGate<'_>) -> anyhow::Result<Vec<release::RecordedMember>> {
        let baseline = CompositionBaseline {
            previous_cut: None,
            newest_write: Some(RecordedCut {
                kind: self.kind,
                name: self.name.clone(),
                commit: self.write.clone(),
                members: self.members.clone(),
            }),
        };
        let check = release::uncarried_recorded_members(
            gate.opened,
            gate.path,
            &mut release::CutSubject::Committed(&self.published),
            &release::CompositionDelta {
                baseline: &baseline,
                parents: gate.parents,
                base: gate.base,
                trunk: gate.trunk,
                tips: gate.tips,
                fixes: &release::SeriesFixes::default(),
            },
        )?;
        Ok(check.dropped)
    }

    /// Record the event, its text naming `drops` ([`Self::recorded_text`]),
    /// and print its line.
    fn record(
        &self,
        (fork, bound): (&Fork<'_>, Bound<'_>),
        drops: &[release::RecordedMember],
    ) -> anyhow::Result<()> {
        let text = self.recorded_text(drops);
        scribe_for(fork, bound)?.record(&Draft {
            evidence: self.evidence(),
            ..Draft::event(Some(&self.name), text.clone(), None)
        })?;
        // The branch and each member's name are read from the repository.
        println!("{}: {}", fork.name, inline_human_text(&text));
        Ok(())
    }
}

/// The withdrawal `cut --corrects WRITE` asks for, or the refusal's exit
/// after saying why, writing nothing.
///
/// WRITE must name the newest recorded write of `name`, the fixed branch,
/// in `ledger`; the composition in hand, `previous`, must be the publish
/// remote's commit of the branch, or with `--at` (`at`) the commit it names,
/// which the publish remote holds ([`fixed_record_at`]); the publish
/// remote's commit must be neither the write nor a descendant of it, as a
/// write it holds was published; and the write must be abandoned: a visible
/// one is still to be pushed, and nothing here says its members may go.
///
/// Its refusal of a visible write says to keep it by the move of the branch
/// to it only where that move leaves nothing the publish remote holds
/// behind ([`moves_behind_published`]); else to abandon it and run the
/// correction again, which records the published commit, then to carry
/// what the write held past that commit ([`fix_forward`]).
fn correction(
    (repo, opened, entry, tips): (
        &RepoName,
        &knives::jj::Repo,
        &knives::config::RepoEntry,
        &knives::detect::BookmarkTips,
    ),
    (name, corrects, at): (&str, &str, bool),
    (ledger, previous): (&[knives::ledger::Entry], Option<&(BookmarkRef, CommitId)>),
) -> anyhow::Result<Result<Withdrawal, Exit>> {
    let remote = entry.publish_remote();
    let shown = inline_human_text(name);
    let refuse = |why: String| -> anyhow::Result<Result<Withdrawal, Exit>> {
        println!(
            "{repo}: refusing to cut {shown} --corrects {}: {why}; nothing was written",
            shell_word(corrects)
        );
        Ok(Err(Exit::Incomplete))
    };
    let Some(published) = tips.get(&BookmarkRef::Remote {
        branch: BranchName::new(name),
        remote: RemoteName::new(remote),
    }) else {
        return refuse(format!(
            "{shown}@{remote} names no commit, and a correction records that commit"
        ));
    };
    let Some((_, recorded)) = previous.filter(|(_, commit)| at || commit == published) else {
        return refuse(format!(
            "the composition in hand is not {shown}@{remote} ({}), the commit a correction \
             records: `jj bookmark set {} -r {} --allow-backwards` moves it there",
            published.short(),
            shell_word(name),
            published.short()
        ));
    };
    let Some((event, newest)) = knives::release_model::recorded_write_events(ledger, name).next()
    else {
        return refuse(format!("the ledger records no write of {shown}"));
    };
    if !names_commit(corrects, &newest.commit) {
        return refuse(format!(
            "{shown}'s newest write is {}, and --corrects withdraws only that one",
            inline_human_text(&release::described_write(&newest))
        ));
    }
    if opened.resolve_commit(newest.commit.as_str()).is_err() {
        return refuse(format!(
            "this repository does not hold {}, so nothing here can tell that the publish remote \
             never held it; fetch it first",
            inline_human_text(newest.commit.short())
        ));
    }
    if unpushed_newest_write(opened, (ledger, name), published)?.is_none() {
        return refuse(format!(
            "{shown}@{remote} ({}) holds that write or a rewrite of it, so it was published",
            published.short()
        ));
    }
    if opened.is_visible(&newest.commit)? {
        let write = newest.commit.short();
        if moves_behind_published(opened, (ledger, name), (published, &newest.commit))? {
            return refuse(format!(
                "{} is visible, so it is not abandoned, and a correction withdraws only a write \
                 nothing keeps; {shown}@{remote} ({p}) holds commits that it lacks, and knives \
                 never moves {shown} behind its publish remote: abandon it (`jj abandon {write}`) \
                 and run this again, which records {p} and names each member of it that {p} \
                 lacks; then, {}",
                inline_human_text(&release::described_write(&newest)),
                fix_forward(name, published, ("what it held", "")),
                p = published.short()
            ));
        }
        return refuse(format!(
            "{} is visible, so it is not abandoned, and a correction withdraws only a write \
             nothing keeps: to keep it, move {shown} to it (`jj bookmark set {} -r {write} \
             --allow-backwards`) and push it; to withdraw it, abandon it first (`jj abandon \
             {write}`)",
            inline_human_text(&release::described_write(&newest)),
            shell_word(name)
        ));
    }
    Ok(Ok(Withdrawal {
        name: name.to_owned(),
        kind: newest.kind,
        members: knives::release_model::recorded_write_members(event, name),
        write: newest.commit,
        remote: remote.to_owned(),
        published: recorded.clone(),
    }))
}

/// The newest recorded write of `name`, a fixed branch, in `ledger`, where
/// `published`, the publish remote's commit of the branch, never held it:
/// this repository holds the write, and `published` is neither it, nor a
/// descendant of it, nor a rewrite of it (the same change). `None`
/// otherwise, and where this repository does not hold the write: nothing
/// here can tell that write from one a fetch would bring, and the
/// composition gate refuses to be held against it
/// ([`release::CompositionCheck::absent_write`]).
fn unpushed_newest_write(
    opened: &knives::jj::Repo,
    (ledger, name): (&[knives::ledger::Entry], &str),
    published: &CommitId,
) -> anyhow::Result<Option<knives::release_model::RecordedWrite>> {
    let Some((_, newest)) = knives::release_model::recorded_write_events(ledger, name).next()
    else {
        return Ok(None);
    };
    if opened.resolve_commit(newest.commit.as_str()).is_err()
        || opened.is_ancestor(&newest.commit, published)?
        || opened.change_id_of(newest.commit.as_str())?
            == opened.change_id_of(published.as_str())?
    {
        return Ok(None);
    }
    Ok(Some(newest))
}

/// Whether a move of the release `name` to `write`, its newest recorded
/// write, would leave `published`, the publish remote's commit of it,
/// behind: `published` is not the write, nor in the write's history, nor a
/// write of the name that the ledger records or a rewrite of one (the same
/// change), which the newest write supersedes. A commit published outside
/// knives, on the write or beside it, is such a commit, as oh-my-pi's Cut9
/// is on its Cut7; knives never moves a release behind what its publish
/// remote holds, so a refusal then says to record that commit as it is,
/// naming what it lacks, and to carry that past it ([`fix_forward`]).
/// Where this repository does not hold the write, nothing here can read
/// the write's history, so only a recorded write keeps the move. A ledger
/// may give a commit short, so a recorded id names a commit only where it
/// gives at least 12 characters of it, as `--corrects` reads one
/// ([`names_commit`]): an empty or shorter id, as a peer's ledger can
/// carry, names no commit, and never brings the move back.
fn moves_behind_published(
    opened: &knives::jj::Repo,
    (ledger, name): (&[knives::ledger::Entry], &str),
    (published, write): (&CommitId, &CommitId),
) -> anyhow::Result<bool> {
    let held =
        |id: &CommitId| id.as_str().len() >= 12 && opened.resolve_commit(id.as_str()).is_ok();
    if names_commit(write.as_str(), published)
        || (held(write) && opened.is_ancestor(published, write)?)
    {
        return Ok(false);
    }
    let change = opened.change_id_of(published.as_str())?;
    for (_, recorded) in knives::release_model::recorded_write_events(ledger, name) {
        if names_commit(recorded.commit.as_str(), published)
            || (held(&recorded.commit) && opened.change_id_of(recorded.commit.as_str())? == change)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The members of `members`, the newest recorded write's, that `published`,
/// a fixed branch's published commit, holds in its history but not as a
/// parent: the members it carries through a carrier
/// ([`CompletedCut::through_carriers`]). A member this repository does not
/// hold is left out: nothing here can read whether the commit holds it.
fn held_through_carriers(
    opened: &knives::jj::Repo,
    published: &CommitId,
    members: &[CommitId],
) -> anyhow::Result<Vec<CommitId>> {
    let parents = opened.parent_commits(published.as_str())?;
    let mut held = Vec::new();
    for member in members {
        if !parents.contains(member)
            && opened.resolve_commit(member.as_str()).is_ok()
            && opened.is_ancestor(member, published)?
        {
            held.push(member.clone());
        }
    }
    Ok(held)
}

/// The composition that a fixed branch's `cut --at REV` records: REV, under
/// the branch's ref, where it is the publish remote's commit of the branch
/// or in its history; or the refusal's exit after saying why, writing
/// nothing. A record names a commit the publish remote holds, so releases
/// published one on the other are recorded in order, each by its own
/// commit, the oldest first.
fn fixed_record_at(
    repo: &RepoName,
    (opened, entry, tips): (
        &knives::jj::Repo,
        &knives::config::RepoEntry,
        &knives::detect::BookmarkTips,
    ),
    (name, at): (&str, &str),
) -> anyhow::Result<Result<(BookmarkRef, CommitId), Exit>> {
    let remote = entry.publish_remote();
    let shown = inline_human_text(name);
    let refuse = |why: String| -> anyhow::Result<Result<(BookmarkRef, CommitId), Exit>> {
        println!(
            "{repo}: refusing to cut {shown} --at {}: {why}; nothing was written",
            inline_human_text(at)
        );
        Ok(Err(Exit::Incomplete))
    };
    let commit = match opened.resolve_commit(at) {
        Ok(commit) => commit,
        Err(error) => {
            return refuse(format!(
                "it does not resolve to one commit ({})",
                problem_human_text(&error.to_string())
            ));
        }
    };
    let Some(published) = tips.get(&BookmarkRef::Remote {
        branch: BranchName::new(name),
        remote: RemoteName::new(remote),
    }) else {
        return refuse(format!(
            "{shown}@{remote} names no commit, and a record of {shown} names a commit it holds"
        ));
    };
    if commit != *published && !opened.is_ancestor(&commit, published)? {
        return refuse(format!(
            "{shown}@{remote} ({}) does not hold {}, and a record of {shown} names a commit it \
             holds",
            published.short(),
            commit.short()
        ));
    }
    Ok(Ok((BookmarkRef::Local(BranchName::new(name)), commit)))
}

/// Refuse the record of `published`, a commit of the fixed branch `name`
/// none of whose `parents` is an earlier write of it (`writes`), where a
/// parent holds one: nothing in the ledger says which parent is the release
/// it was made on. Each such parent may be a release of the branch published
/// on that write and never recorded, or a member built on it, as a landing
/// whose branch the forge deleted is; a parent's place, its descent and its
/// bookmarks do not tell them apart, so knives names each and records
/// nothing. Whether it refused: `false` where no parent holds a write, as in
/// a composition that holds no release of the branch.
fn refuse_unknown_release_parents(
    (repo, opened): (&RepoName, &knives::jj::Repo),
    (name, published): (&str, &CommitId),
    (parents, writes): (&[CommitId], &EarlierWrites),
) -> anyhow::Result<bool> {
    let shown = inline_human_text(name);
    let mut unknown = Vec::new();
    for parent in parents {
        for write in writes.commits() {
            if opened.is_ancestor(write, parent)? {
                unknown.push((parent, write));
                break;
            }
        }
    }
    for (parent, write) in &unknown {
        println!(
            "{repo}: {}, a parent of {}, holds {shown}'s earlier write {} and is no write the \
             ledger records: a release of {shown} published on it and never recorded, or a \
             member built on it",
            parent.short(),
            published.short(),
            write.short()
        );
    }
    if unknown.is_empty() {
        return Ok(false);
    }
    println!(
        "{repo}: refusing to cut {shown}: no parent of {} is a write of {shown} the ledger \
         records, so knives cannot tell which parent is the release it was made on; if a parent \
         above is that release, record it first (`knives release cut --at <commit>`), then cut \
         again; if each is a member, {} does not merge the release it was made on, and knives \
         records only a release that does; nothing was written",
        published.short(),
        published.short()
    );
    Ok(true)
}

/// What a refusal past the publish remote's commit ([`moves_behind_published`])
/// names once the write has recorded `published`, the publish remote's
/// commit of the release `name`, as it is: a merge of `published` and a
/// branch that carries `what`, made after `fetch` (each fetch a fix asks
/// for, [`unread_fetch`]), with the working copy stepped off it, pushed,
/// then the cut again. `published` is then the newest recorded write, so
/// the cut records that merge with `published` as its carrier
/// ([`knives::release_model::EarlierWrites`]) and the branch as a member,
/// and reads the branch's lines against `published`. Nothing the remote
/// holds is rewritten or left behind, and nothing is merged into a commit
/// the ledger has not recorded. A push refuses a commit with no
/// description, so the merge is given one. The name is a ledger entry's
/// subject: shown escaped, and quoted for the shell inside the commands.
fn fix_forward(name: &str, published: &CommitId, (what, fetch): (&str, &str)) -> String {
    let (shown, at) = (inline_human_text(name), published.short());
    format!(
        "to carry {what}, {fetch}make {shown} a merge of {at} and a branch that carries it (`jj \
         new {at} <branch> -m {}`, `jj bookmark set {} -r @`, then `jj new 'trunk()'`), push \
         {shown}, then cut again, which records that merge with {at} as its carrier",
        shell_word(&format!("{name}, fixed forward on {at}")),
        shell_word(name)
    )
}

/// The plan's exit, and the consumer pins it scanned. The cut judges the pins
/// itself, against the release it compares the candidate with: when every pin
/// of that release is frozen, a new dated name is the only editable
/// composition, so the identical-composition refusal must stand down for it.
fn release_plan_exit(
    fork: &Fork<'_>,
    fetched: release::Fetched,
    consumers: &release::ConsumerInputs<'_>,
    opened: &knives::jj::Repo,
) -> anyhow::Result<(Exit, Vec<knives::pins::Pin>)> {
    let entry = fork.entry;
    let plan = fetched.plan(fork, consumers)?;
    println!("{}", release::render(&plan));
    let mut exit = release::exit_for(&plan);
    if let Some(lag) = release::trunk_lag(opened, plan.release.as_deref(), &entry.trunk_ref()) {
        // The line names the release, a ref read from the repository.
        println!("  !! {}", inline_human_text(&lag));
        exit = exit.worst(Exit::Findings);
    }
    Ok((exit, plan.pins))
}

/// Why a cut under a new dated name may start identical to `previous`, at
/// `tip` here and `published` on the publish remote, or `None`: every pin of
/// a dated or series release is frozen, or it is published (a tag or a
/// `record: published` note names its commit), so it cannot be edited in
/// place.
fn verbatim_cut_reason(
    opened: &knives::jj::Repo,
    (pins, scheme, ledger): (
        &[knives::pins::Pin],
        &ReleaseScheme,
        &[knives::ledger::Entry],
    ),
    (previous, tip, published): (&BookmarkRef, &CommitId, Option<&CommitId>),
) -> Result<Option<String>, knives::jj::JjError> {
    let frozen = matches!(scheme, ReleaseScheme::Dated | ReleaseScheme::Series(_))
        && release::repair_effect(pins, previous.branch(), published)
            == release::RepairEffect::NewDatedName;
    let evidence = release::published_release(
        opened,
        ledger,
        release::ReleaseAt {
            scheme,
            name: previous.branch().as_str(),
            commits: &[published, Some(tip)],
        },
    )?;
    // The previous release is a name read from the repository, and the
    // evidence names a tag on it or quotes a ledger note.
    let shown = inline_human_text(previous.branch().as_str());
    Ok(if frozen {
        Some(format!("every pin of {shown} is frozen"))
    } else {
        evidence.map(|evidence| format!("{shown} is published ({})", inline_human_text(&evidence)))
    })
}

/// Say what the audit found; refuse when it failed.
///
/// Inconclusive members are reported without failing the cut: a member
/// whose own path holds an unresolved conflict cannot be read there. Missing
/// or unexplained content refuses the cut, because naming it would look
/// exactly like success while work is gone. The refused candidate was never
/// a published operation, so nothing is abandoned and nothing needs cleanup.
/// A divergence carried from the release in hand reads by how that release
/// stands to its recorded writes (`carried_from`), and an unexplained file
/// names what the drift was measured from (`drift_from`, the audit's
/// [`release::AuditContext::drift_from`]). Under each inconclusive and
/// missing row, the lines name what the member lacks
/// ([`release::lost_lines`]); after the rows, each line of a member that
/// the cut holds only by its words is named, whether or not the audit
/// passed ([`release::labelled_words_lines`]). Where the missing members are
/// drops `--allow-drop` may name (`drops`, [`waived_members`]), the refusal
/// says so and gives the flag of each before its last line.
fn report_cut_audit(
    repo: &RepoName,
    audit: &release::CutAudit,
    (carried_from, drift_from): (
        release::CarriedFrom,
        Option<(&CommitId, release::DriftBase)>,
    ),
    drops: &[String],
) -> Option<Exit> {
    for name in &audit.carried {
        println!("{}", release::carried_row(name, carried_from));
    }
    // Each name is a member's bookmark, read from the repository.
    for name in &audit.inconclusive {
        println!(
            "  {}: content check inconclusive (an unresolved conflict; re-check after resolving \
             it)",
            inline_human_text(name)
        );
        for line in audit.lost_lines_of(name) {
            println!("{line}");
        }
    }
    if audit.passed() {
        for line in audit.words_lines() {
            println!("{line}");
        }
        return None;
    }
    for name in &audit.missing {
        println!(
            "  !! {}: the cut tree is missing or diverges from the member's content",
            inline_human_text(name)
        );
        for line in audit.lost_lines_of(name) {
            println!("{line}");
        }
    }
    // Only a drift check finds a file no member explains.
    if let Some((_, base)) = drift_from {
        for file in &audit.unexplained {
            println!("{}", release::unexplained_row(file, base));
        }
    }
    for line in audit.words_lines() {
        println!("{line}");
    }
    if !drops.is_empty() {
        println!(
            "{repo}: refusing to record the published commit without those members' lines; \
             re-run with --allow-drop naming each member to state the drop is intended"
        );
        println!("{}", drop_flags_line(drops));
    }
    println!("{repo}: cut discarded; nothing was written at all. Fix the inputs and re-cut.");
    Some(Exit::Incomplete)
}

/// The fork's lock a cut holds, which a refusal lets go of before it asks the
/// remotes where a commit that this repository lacks can be fetched
/// ([`Remotes`]): a refusal writes nothing after it, so no other writer of
/// the fork waits on those remotes.
struct CutLock(std::cell::RefCell<Option<ForkLock>>);

impl CutLock {
    const fn of(held: Option<ForkLock>) -> Self {
        Self(std::cell::RefCell::new(held))
    }

    /// Let the lock go, where it is still held.
    fn release(&self) {
        drop(self.0.borrow_mut().take());
    }
}

/// The composition gate's inputs: everything the recorded-member check reads
/// beside the cut subject.
struct CompositionGate<'a> {
    /// What the cut subject is held against, read from the fork's ledger
    /// ([`Self::read_baseline`]).
    baseline: CompositionBaseline,
    /// The release cut outside knives that `cut NAME --at` records: NAME and
    /// the commit it is recorded at. `None` for a cut knives writes, and for
    /// the fixed branch's published composition that a cut records.
    record: Option<(&'a str, &'a CommitId)>,
    opened: &'a knives::jj::Repo,
    /// The fork's registry entry, whose remotes a refusal reads for the
    /// fetch of a commit this repository does not hold ([`Remotes`]).
    entry: &'a knives::config::RepoEntry,
    /// The cut's lock, which such a refusal lets go of before it reads them.
    lock: &'a CutLock,
    /// The ledger the baseline was read from, whose recorded writes a
    /// refusal's remedy reads ([`Self::published_past`]).
    ledger: &'a [knives::ledger::Entry],
    path: &'a std::path::Path,
    parents: &'a [knives::ids::CommitId],
    base: &'a knives::ids::CommitId,
    trunk: &'a knives::ids::CommitId,
    tips: &'a knives::detect::BookmarkTips,
    /// The fixes by hand that the series holds: each `adopt` of any of its
    /// releases, at each file where its hold has not ended, each held
    /// against its reference, and each adopt this repository does not hold
    /// ([`release::adopted_fixes`]).
    fixes: release::SeriesFixes,
}

impl CompositionGate<'_> {
    /// The baseline of the gate of `record` ([`Self::record`]), read from
    /// `ledger` in `entry`'s series: the name a record records is aside, as
    /// no write of it is its own baseline. With it, the fixes by hand that
    /// the series holds ([`release::adopted_fixes`]), held against the
    /// series' writes ([`knives::release_model::series_writes`]), read in
    /// `opened` with `tips`.
    fn read_baseline(
        (opened, entry): (&knives::jj::Repo, &knives::config::RepoEntry),
        (ledger, tips): (&[knives::ledger::Entry], &knives::detect::BookmarkTips),
        record: Option<(&str, &CommitId)>,
    ) -> anyhow::Result<(CompositionBaseline, release::SeriesFixes)> {
        let except = record.map(|(name, _)| name);
        let baseline = CompositionBaseline::read(ledger, entry, except);
        let writes = knives::release_model::series_writes(ledger, &entry.release_scheme(), except);
        let fixes = release::adopted_fixes(opened, entry, (ledger, tips), &writes)?;
        Ok((baseline, fixes))
    }

    /// The records of the baseline in the order the gate's messages name
    /// them, with the record the gate holds, or `None` when the ledger
    /// records neither. A cut names its previous cut first, the composition
    /// it duplicates; a record names the newest write first, the release it
    /// follows. The other record names each member that only it holds.
    fn held(&self) -> Option<HeldRecords<'_>> {
        let cut = self
            .baseline
            .previous_cut
            .as_ref()
            .map(HeldRecord::PreviousCut);
        let write = self
            .baseline
            .newest_write
            .as_ref()
            .map(HeldRecord::NewestWrite);
        let order = if self.record.is_some() {
            [write, cut]
        } else {
            [cut, write]
        };
        let mut records = order.into_iter().flatten();
        let first = records.next()?;
        Some(HeldRecords {
            first,
            other: records.next(),
            record: self.record,
            past: None,
        })
    }

    /// The publish remote's commit of `write`'s release, the newest write,
    /// where a move of the release to that write would leave it behind
    /// ([`moves_behind_published`]), for a refusal's remedy; `None` where
    /// the move leaves nothing behind or the remote has no commit of it.
    fn published_past(&self, write: &RecordedCut) -> anyhow::Result<Option<PublishedPast<'_>>> {
        let remote = self.entry.publish_remote();
        let Some(commit) = self.tips.get(&BookmarkRef::Remote {
            branch: BranchName::new(write.name.as_str()),
            remote: RemoteName::new(remote),
        }) else {
            return Ok(None);
        };
        if !moves_behind_published(
            self.opened,
            (self.ledger, &write.name),
            (commit, &write.commit),
        )? {
            return Ok(None);
        }
        Ok(Some(PublishedPast {
            remote,
            commit,
            held: self.opened.resolve_commit(write.commit.as_str()).is_ok(),
        }))
    }
}

/// A record of a composition gate's [`CompositionBaseline`], as its messages
/// name it.
#[derive(Clone, Copy)]
enum HeldRecord<'a> {
    /// The newest cut event of the series.
    PreviousCut(&'a RecordedCut),
    /// The newest recorded write of the newest release the ledger records in
    /// the series: its cut, its newest edit or an `adopt`.
    NewestWrite(&'a RecordedCut),
}

/// The words for each kind of record, which every message of the gate and
/// the cut event use.
impl<'a> HeldRecord<'a> {
    const fn recorded(self) -> &'a RecordedCut {
        match self {
            Self::PreviousCut(recorded) | Self::NewestWrite(recorded) => recorded,
        }
    }

    /// What the record is called, `name` its release's name as the caller
    /// renders it: `previous cut NAME`, or `NAME's newest recorded write (an
    /// edit)`.
    fn title(self, name: &str) -> String {
        match self {
            Self::PreviousCut(_) => format!("previous cut {name}"),
            Self::NewestWrite(write) => {
                let kind = match write.kind {
                    RecordedWriteKind::Cut => "a cut",
                    RecordedWriteKind::Edit => "an edit",
                };
                format!("{name}'s newest recorded write ({kind})")
            }
        }
    }

    /// The record and how many members it holds, as the cut event restates
    /// it, the name raw: `previous cut NAME recorded N member(s)`, or `NAME's
    /// newest recorded write (an edit) holds N member(s)`.
    fn restated(self) -> String {
        let recorded = self.recorded();
        let verb = match self {
            Self::PreviousCut(_) => "recorded",
            Self::NewestWrite(_) => "holds",
        };
        format!(
            "{} {verb} {} member(s)",
            self.title(&recorded.name),
            recorded.members.len()
        )
    }

    /// The record and its commit, each given by `render`: `NAME's newest
    /// recorded write (a cut) 0123456789ab`. The name and the commit are a
    /// ledger entry's subject and first evidence string, so a message escapes
    /// them ([`inline_human_text`]) and the cut event keeps them raw.
    fn with_commit(self, render: fn(&str) -> String) -> String {
        let recorded = self.recorded();
        format!(
            "{} {}",
            self.title(&render(&recorded.name)),
            render(recorded.commit.short())
        )
    }
}

/// The records a composition gate holds the cut subject against, in the
/// order its messages name them ([`CompositionGate::held`]), and the record
/// the gate holds.
#[derive(Clone, Copy)]
struct HeldRecords<'a> {
    first: HeldRecord<'a>,
    other: Option<HeldRecord<'a>>,
    /// The release cut outside knives that the gate holds
    /// ([`CompositionGate::record`]): `None` for a cut.
    record: Option<(&'a str, &'a CommitId)>,
    /// The publish remote's commit of the newest write's release where a
    /// move of the release to that write would leave it behind, read only
    /// for a refusal's remedy ([`CompositionGate::published_past`]).
    past: Option<PublishedPast<'a>>,
}

impl<'a> HeldRecords<'a> {
    /// Whether the first record names `commit`: it holds it, or there is no
    /// other record.
    fn first_names(self, commit: &CommitId) -> bool {
        self.other.is_none() || self.first.recorded().members.contains(commit)
    }

    /// The record that names `commit` ([`Self::first_names`]).
    fn holder(self, commit: &CommitId) -> HeldRecord<'a> {
        match self.other {
            Some(other) if !self.first_names(commit) => other,
            _ => self.first,
        }
    }

    /// The names of the members of `members` that the first record names,
    /// and the other record with the names of the rest.
    fn split<'m>(
        self,
        members: &'m [release::RecordedMember],
    ) -> (Vec<&'m str>, Option<(HeldRecord<'a>, Vec<&'m str>)>) {
        let (mut first, mut rest) = (Vec::new(), Vec::new());
        for member in members {
            if self.first_names(&member.commit) {
                first.push(member.name.as_str());
            } else {
                rest.push(member.name.as_str());
            }
        }
        (first, self.other.map(|other| (other, rest)))
    }

    /// The newest write among the records, when the ledger records one.
    fn newest_write(self) -> Option<HeldRecord<'a>> {
        std::iter::once(self.first)
            .chain(self.other)
            .find(|held| matches!(held, HeldRecord::NewestWrite(_)))
    }

    /// What follows `N member(s)` where a message names members of `source`
    /// that the subject lacks, `name` the release's name escaped: `the
    /// previous cut NAME recorded`; for a cut, `which NAME's newest recorded
    /// write (an edit) holds`; for a record, which follows that release, `of
    /// NAME, the newest release that the ledger records in the series`.
    fn holds(self, source: HeldRecord<'_>, name: &str) -> String {
        match (source, self.record) {
            (HeldRecord::PreviousCut(_), _) => format!("the {} recorded", source.title(name)),
            (HeldRecord::NewestWrite(_), Some(_)) => {
                format!("of {name}, the newest release that the ledger records in the series")
            }
            (HeldRecord::NewestWrite(_), None) => format!("which {} holds", source.title(name)),
        }
    }

    /// What an `--allow-drop` line says goes ahead: `cutting`, or `recording
    /// NAME at COMMIT`, escaped. The name is the caller's, kept to the
    /// series' grammar, whose label takes any character but `/`.
    fn doing(self) -> String {
        self.record.map_or_else(
            || "cutting".to_owned(),
            |(name, at)| format!("recording {} at {}", inline_human_text(name), at.short()),
        )
    }

    /// A refusal up to the subject's verb, escaped: `REPO: refusing to cut:
    /// this cut`, or `REPO: refusing to record NAME at COMMIT: it`.
    fn refusing(self, repo: &RepoName) -> String {
        self.record.map_or_else(
            || format!("{repo}: refusing to cut: this cut"),
            |(name, at)| {
                format!(
                    "{repo}: refusing to record {} at {}: it",
                    inline_human_text(name),
                    at.short()
                )
            },
        )
    }

    /// What the gate holds: `this cut`, or `this record`.
    const fn subject(self) -> &'static str {
        if self.record.is_some() {
            "this record"
        } else {
            "this cut"
        }
    }
}

/// The publish remote's commit of the newest write's release, where a move
/// of the release to that write would leave it behind
/// ([`moves_behind_published`]): a refusal then never names that move.
#[derive(Clone, Copy)]
struct PublishedPast<'a> {
    /// The publish remote, a registry value.
    remote: &'a str,
    /// Its commit of the release.
    commit: &'a CommitId,
    /// Whether this repository holds the write: where it does not, nothing
    /// here can read whether the write holds the commit.
    held: bool,
}

impl PublishedPast<'_> {
    /// What a refusal names in place of the move of the release to `write`,
    /// its newest recorded write, where `subject` (what the gate holds)
    /// lacks what that write holds, after `unread`, each fetch a fix asks
    /// for ([`unread_fetch`]). Where this repository holds the write: record
    /// the published commit as it is, each drop named with `--allow-drop`,
    /// then carry what it lacks past it ([`fix_forward`]), so every step is
    /// recorded and nothing the remote holds is rewritten or left behind.
    /// Where it does not: the fetch and the cut again, which reads the write
    /// then. The release and the write are a ledger entry's subject and first
    /// evidence string: shown escaped.
    fn remedy(self, (write, subject): (&RecordedCut, &str), unread: &str) -> String {
        let (name, at) = (
            inline_human_text(&write.name),
            inline_human_text(write.commit.short()),
        );
        let (remote, published) = (self.remote, self.commit.short());
        if self.held {
            return format!(
                "{name}@{remote} ({published}) holds commits that {at}, its newest recorded \
                 write, lacks, and knives never moves {name} behind its publish remote: re-run \
                 with --allow-drop naming each drop below, which records {published} as it is \
                 and names each drop; then, {}",
                fix_forward(
                    &write.name,
                    self.commit,
                    (&format!("what {subject} lacks"), unread)
                )
            );
        }
        let fetch = if unread.is_empty() {
            "fetch, then "
        } else {
            unread
        };
        format!(
            "{fetch}cut again: this repository does not hold {at}, {name}'s newest recorded \
             write, so it cannot tell whether that write holds {name}@{remote} ({published}), and \
             knives never moves {name} behind its publish remote"
        )
    }
}

/// Hold the cut subject against the gate's [`CompositionBaseline`]: each
/// member the previous cut's event or the newest write records.
///
/// `Err` is the refusal, already reported. `Ok` carries what the check
/// found, for the cut event to restate. The subject goes ahead without the
/// drops it finds only where `allow_drop` names each of them.
fn recorded_composition_check(
    repo: &RepoName,
    subject: &mut release::CutSubject<'_>,
    gate: &CompositionGate<'_>,
    allow_drop: &AllowDrop,
) -> anyhow::Result<Result<release::CompositionCheck, Exit>> {
    let Some(held) = gate.held() else {
        return Ok(Ok(release::CompositionCheck::default()));
    };
    let check = release::uncarried_recorded_members(
        gate.opened,
        gate.path,
        subject,
        &release::CompositionDelta {
            baseline: &gate.baseline,
            parents: gate.parents,
            base: gate.base,
            trunk: gate.trunk,
            tips: gate.tips,
            fixes: &gate.fixes,
        },
    )?;
    let allowed = check.refuses() && allow_drop.names_every(held, &check);
    let on_drop = match (allowed, &*subject) {
        (true, _) => DropDisposition::Allowed,
        (false, release::CutSubject::Candidate(_)) => DropDisposition::RefuseDiscardingCandidate,
        (false, release::CutSubject::Committed(_)) => DropDisposition::RefuseRecordingNothing,
    };
    // The fork's remotes are read only where the refusal names the fetch of
    // a commit this repository does not hold, and then once, after the cut
    // lets its lock go ([`Remotes`]).
    let remotes = Remotes::of(gate.entry, gate.lock);
    // Drops that `--allow-drop` names are stated once every gate has asked
    // and each value has named a drop ([`AllowDrop::settle`]).
    if allowed {
        allow_drop.state(uncarried_cut_lines(repo, held, &check, (on_drop, &remotes)));
        return Ok(Ok(check));
    }
    // A refusal's remedy reads where the publish remote's commit of the
    // newest write's release stands against that write.
    let past = held
        .newest_write()
        .filter(|_| check.refuses())
        .map(|write| gate.published_past(write.recorded()))
        .transpose()?
        .flatten();
    let held = HeldRecords { past, ..held };
    if let Some(exit) = report_uncarried_cut(repo, held, &check, (on_drop, &remotes)) {
        return Ok(Err(exit));
    }
    Ok(Ok(check))
}

/// What a cut does about recorded members that it lacks.
#[derive(Clone, Copy)]
enum DropDisposition {
    /// `--allow-drop` names each drop: state the drops and carry on.
    Allowed,
    /// Refuse; the candidate commit was never published, so it is simply gone.
    RefuseDiscardingCandidate,
    /// Refuse; the composition is already on origin, so nothing changes there.
    RefuseRecordingNothing,
}

/// Say what the recorded-composition check found; refuse when members are
/// gone, when this repository does not hold the newest write, or when the
/// subject lacks a fix by hand that the series holds, an `adopt` of any of
/// its releases, at a file where its hold has not ended
/// ([`release::CompositionCheck::refuses`]).
///
/// Inconclusive members are reported without refusing, for the same reason the
/// audit's are: a member whose own path holds an unresolved conflict cannot be
/// read there. A member the cut subject does not carry refuses the cut,
/// the recorded writes' ledger events are the only surviving record of the
/// composition — every edit moves the bookmark and abandons the commit it
/// replaced. A refused candidate was never published and a refused
/// record leaves origin as it was, so nothing is abandoned and nothing needs
/// cleanup.
fn report_uncarried_cut(
    repo: &RepoName,
    held: HeldRecords<'_>,
    check: &release::CompositionCheck,
    (on_drop, remotes): (DropDisposition, &Remotes<'_>),
) -> Option<Exit> {
    for line in uncarried_cut_lines(repo, held, check, (on_drop, remotes)) {
        println!("{line}");
    }
    (check.refuses() && !matches!(on_drop, DropDisposition::Allowed)).then_some(Exit::Incomplete)
}

/// The lines [`report_uncarried_cut`] prints, which name each member with
/// the record that names it ([`HeldRecords::holder`]), the newest write
/// that this repository does not hold, and each adopt whose fix the subject
/// lacks, with the files where it lacks it ([`fix_lines`]). A refusal's
/// remedy names the fetch of a commit that this repository does not hold
/// only where one of `remotes` has a branch or tag at it ([`fix_remedy`]).
/// Under each inconclusive member, and after a refusal or the `--allow-drop`
/// lines for each member they name, the lines name what the member lacks
/// ([`lacking_lines`]); last, whether or not the check refuses, each line
/// of a member that the subject holds only by its words
/// ([`release::labelled_words_lines`]).
///
/// The recorded release's name and its members are a ledger entry's subject
/// and evidence, which a peer's pulled ledger can carry any text in; a member
/// is rendered with the bookmark that holds it, a name read from the
/// repository. Each is escaped here; the cut event keeps them raw.
fn uncarried_cut_lines(
    repo: &RepoName,
    held: HeldRecords<'_>,
    check: &release::CompositionCheck,
    (on_drop, remotes): (DropDisposition, &Remotes<'_>),
) -> Vec<String> {
    let mut lines = Vec::new();
    for (member, twin) in &check.twins {
        lines.push(format!(
            "{repo}: {} is carried by {}, which has the same tree",
            inline_human_text(member),
            twin.short()
        ));
    }
    for member in &check.inconclusive {
        let holder = held.holder(&member.commit);
        lines.push(format!(
            "  {}: recorded by the {}; carry check inconclusive (an unresolved conflict; \
             re-check after resolving it)",
            inline_human_text(&member.name),
            holder.title(&inline_human_text(&holder.recorded().name))
        ));
        lines.extend(check.lost_lines_of(&member.commit));
    }
    // A fix whose file the cut leaves conflicted is unanswerable until the
    // conflict is resolved: the person resolves it in place and adopts.
    for fix in &check.inconclusive_fixes {
        lines.push(format!(
            "  {}{}: carry check inconclusive (the cut leaves those files conflicted; resolve them \
             in place, then run `knives release adopt`)",
            fix_title((&fix.name, &fix.commit), inline_human_text),
            fix_files(&fix.files, inline_human_text)
        ));
    }
    // An adopt this repository does not hold holds nothing here.
    for adopt in &check.absent_adopts {
        lines.push(format!(
            "{repo}: {} holds nothing here: this repository does not hold {}, so nothing here \
             can tell what it changed; fetch it to hold {} against it",
            fix_title((&adopt.name, &adopt.commit), inline_human_text),
            inline_human_text(adopt.commit.short()),
            held.subject()
        ));
    }
    if check.refuses() {
        lines.extend(refusal_lines(repo, held, check, (on_drop, remotes)));
    }
    lines.extend(check.words_lines());
    lines
}

/// The lines of [`uncarried_cut_lines`] for a check that refuses: each
/// member the subject lacks with the record that names it
/// ([`dropped_lines`]), the newest write that this repository does not
/// hold, and each adopt whose fix the subject lacks ([`fix_lines`]); under
/// `--allow-drop` each as a statement, else as a refusal with its remedy;
/// after them, in both cases, what each member they name lacks
/// ([`lacking_lines`]).
fn refusal_lines(
    repo: &RepoName,
    held: HeldRecords<'_>,
    check: &release::CompositionCheck,
    (on_drop, remotes): (DropDisposition, &Remotes<'_>),
) -> Vec<String> {
    let mut lines = Vec::new();
    let absent = held.newest_write().filter(|_| check.absent_write);
    let allowed = matches!(on_drop, DropDisposition::Allowed);
    let (first, other) = held.split(&check.dropped);
    let mut sources = Vec::new();
    for (source, dropped) in std::iter::once((held.first, first)).chain(other) {
        if !dropped.is_empty() {
            lines.extend(dropped_lines(repo, (held, source), &dropped, allowed));
            sources.push(source);
        }
    }
    if let Some(write) = absent {
        lines.push(if allowed {
            format!(
                "{repo}: --allow-drop: {} without holding it against {}, which this repository \
                 does not hold",
                held.doing(),
                write.with_commit(inline_human_text)
            )
        } else {
            format!(
                "{} cannot be held against {}: this repository does not hold that commit, so it \
                 cannot tell what that write carries",
                held.refusing(repo),
                write.with_commit(inline_human_text)
            )
        });
    }
    lines.extend(fix_lines(repo, held, &check.dropped_fixes, allowed));
    if !allowed {
        let drops = drop_names(held, check);
        lines.push(refusal_remedy(
            held,
            (&sources, absent, &check.dropped_fixes),
            (on_drop, remotes, !drops.is_empty()),
        ));
        if !drops.is_empty() {
            lines.push(drop_flags_line(&drops));
        }
    }
    lines.extend(lacking_lines(check));
    lines
}

/// What the members that a refusal or `--allow-drop` names lack, after the
/// whole block, so that each line of the block stays as it reads: under the
/// line `lines each member lacks:`, each such member that the check read,
/// by its name, with the lines that name what it lacks
/// ([`release::lost_lines`]).
/// None where the check read no line that such a member lacks.
fn lacking_lines(check: &release::CompositionCheck) -> Vec<String> {
    let mut lines = Vec::new();
    for member in &check.dropped {
        let lost = check.lost_lines_of(&member.commit);
        if !lost.is_empty() {
            lines.push(format!("    {}:", inline_human_text(&member.name)));
            lines.extend(lost);
        }
    }
    if !lines.is_empty() {
        lines.insert(0, "  lines each member lacks:".to_owned());
    }
    lines
}

/// The lines of [`uncarried_cut_lines`] that name each adopt of `fixes`,
/// whose fix the subject lacks ([`fix_title`]), with the files where it
/// lacks it ([`fix_files`]). `allowed` says whether `--allow-drop` names
/// each drop and lets the subject go ahead.
fn fix_lines(
    repo: &RepoName,
    held: HeldRecords<'_>,
    fixes: &[release::LackedFix],
    allowed: bool,
) -> Vec<String> {
    fixes
        .iter()
        .map(|fix| {
            let named = format!(
                "{}{}",
                fix_title((&fix.name, &fix.commit), inline_human_text),
                fix_files(&fix.files, inline_human_text)
            );
            if allowed {
                format!("{repo}: --allow-drop: {} without {named}", held.doing())
            } else {
                format!("{} does not carry {named}", held.refusing(repo))
            }
        })
        .collect()
}

/// `the fix by hand of NAME's adopt COMMIT`, the
/// adopt's release and commit each given by `render`. They are a ledger
/// entry's subject and first evidence string, so a message escapes them
/// ([`inline_human_text`]) and the cut event keeps them raw.
fn fix_title((name, commit): (&str, &CommitId), render: fn(&str) -> String) -> String {
    format!(
        "the fix by hand of {}'s adopt {}",
        render(name),
        render(commit.short())
    )
}

/// What follows [`fix_title`]: `, in N file(s): F, G`, the files of the fix
/// that the subject lacks, each given by `render`. They are the
/// repository's paths, so a message escapes them ([`inline_human_text`])
/// and the cut event keeps them raw.
fn fix_files(files: &[release::HeldFile], render: fn(&str) -> String) -> String {
    let shown: Vec<String> = files.iter().map(|file| render(&file.path)).collect();
    format!(", in {} file(s): {}", shown.len(), shown.join(", "))
}

/// The line that closes a refusal of [`uncarried_cut_lines`]: what each
/// refused finding asks for, or `--allow-drop`, whose value for each drop
/// the next line gives ([`drop_flags_line`]). `sources` are the records
/// whose members the subject lacks, `absent` the newest write where this
/// repository does not hold it, and `fixes` the fixes by hand that the
/// series holds the subject lacks.
///
/// A record is built again after a fetch when this repository does not
/// hold that write, or a write that a lacked file's reference needs, where
/// one of `remotes` has a branch or tag at it ([`unread_fetch`]). A cut gets
/// back a member of the previous cut with `include`, after the move to the
/// newest write where the refusal names one, and after the `adopt` that
/// takes the move's place where the refusal names that too. A member only
/// the newest write holds, a write not held here, and a fix that the newest
/// write carries and the cut lacks mean the release here is older than that
/// write, as on a machine that has not fetched it: so the cut is made again
/// once that write's release is moved to it, after the fetch of each write
/// that [`unread_fetch`] names, and an `include` on the older commit would
/// write a second, divergent write. A write not held here may be one no
/// fetch brings, so both refusals then name `adopt` too ([`adopt_remedy`]).
/// A fix that the subject changes on purpose is kept by an `adopt` of the
/// new content ([`fix_adopt_remedy`]). A record is asked for a commit that
/// carries a fix only where a commit can carry it
/// ([`FixRemedy::commit`]): a fix whose every lacked file has no reference
/// here keeps the adopt of the new content, which gives those files one.
/// What each fix asks for is [`fix_remedy`]'s, read once for the refusal.
/// `--allow-drop` closes the line only where the refusal names a drop
/// (`drops`): a newest write not held here is no drop.
///
/// The move is never named where the publish remote's commit of the newest
/// write's release holds commits that the write lacks ([`HeldRecords::past`]):
/// it would leave that commit behind. The record of that commit as it is,
/// each drop named, then the fix-forward past it, takes its place, or, where this
/// repository does not hold the write, the fetch and the cut again
/// ([`PublishedPast::remedy`]); and the edit by hand is not named, as it
/// would rewrite the commit the remote holds.
fn refusal_remedy(
    held: HeldRecords<'_>,
    (sources, absent, fixes): (
        &[HeldRecord<'_>],
        Option<HeldRecord<'_>>,
        &[release::LackedFix],
    ),
    (on_drop, remotes, drops): (DropDisposition, &Remotes<'_>, bool),
) -> String {
    let asks: Vec<FixRemedy<'_>> = fixes.iter().map(|fix| fix_remedy(fix, remotes)).collect();
    let adopt_fixes = held
        .newest_write()
        .filter(|_| !asks.is_empty() && held.past.is_none())
        .map(fix_adopt_remedy);
    if held.record.is_some() {
        let remedies: Vec<String> = record_remedy((sources, absent, &asks))
            .into_iter()
            .chain(adopt_fixes)
            .collect();
        return closing_line("nothing was recorded", &remedies, (remotes.unread(), drops));
    }
    let outcome = match on_drop {
        DropDisposition::RefuseDiscardingCandidate => "the candidate was discarded",
        DropDisposition::Allowed | DropDisposition::RefuseRecordingNothing => {
            "nothing was recorded"
        }
    };
    // `include` follows the move to the newest write, and the `adopt` that
    // takes the move's place where the fetch does not bring that write: the
    // release here is older than that write, and an `include` on it would
    // write a second, divergent write.
    let restore = sources
        .iter()
        .any(|source| matches!(source, HeldRecord::PreviousCut(_)))
        .then(|| {
            "`knives release include <branch>` restores a member the previous cut recorded"
                .to_owned()
        });
    let mut remedies = Vec::new();
    let newer = sources
        .iter()
        .copied()
        .find(|source| matches!(source, HeldRecord::NewestWrite(_)))
        .or(absent)
        .or_else(|| held.newest_write().filter(|_| !asks.is_empty()));
    if let Some(write) = newer {
        let recorded = write.recorded();
        let (name, commit) = (&recorded.name, recorded.commit.short());
        let unread = unread_fetch(&asks);
        remedies.push(held.past.map_or_else(
            || {
                let fetch = if unread.is_empty() {
                    "fetch, then "
                } else {
                    unread.as_str()
                };
                format!(
                    "{fetch}move {} to {}, its newest recorded write: `jj bookmark set {} -r {} \
                     --allow-backwards`",
                    inline_human_text(name),
                    inline_human_text(commit),
                    shell_word(name),
                    shell_word(commit)
                )
            },
            |past| past.remedy((recorded, held.subject()), &unread),
        ));
    }
    if let Some(write) = absent {
        remedies.push(adopt_remedy(write));
    }
    remedies.extend(restore);
    remedies.extend(adopt_fixes);
    // Past the published commit the remedy already says to name each drop.
    let drops = drops && !held.past.is_some_and(|past| past.held);
    closing_line(outcome, &remedies, (remotes.unread(), drops))
}

/// What a refusal asks for about a fix by hand that the subject lacks
/// ([`fix_remedy`]): a commit that carries the fix at the files that the
/// subject lacks, where `commit` says that a commit can, after `fetches`,
/// the fetch of each write that a lacked file's reference needs; and the
/// adopt that keeps the subject's new content there instead
/// ([`fix_adopt_remedy`]).
#[derive(Clone, PartialEq, Eq)]
struct FixRemedy<'r> {
    commit: bool,
    fetches: Vec<Fetch<'r>>,
}

/// The fetch of `commit`, which this repository does not hold, from
/// `remote`, which has `reference` at it now ([`Remotes::at`]).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fetch<'r> {
    remote: &'r str,
    reference: &'r str,
    commit: &'r CommitId,
}

/// What a refusal asks for about `fix`, a fix by hand that the subject
/// lacks: the one place that reads the shape of a lacked fix for its
/// remedy ([`refusal_remedy`]).
///
/// A fix lacked at its files keeps the adopt of the subject's new content
/// there. A commit carries it at a file of the adopt's change
/// ([`release::HeldFile::replayed`]), which a commit that the change
/// replays empty onto carries, or at a file with a reference
/// ([`release::FixReference::Write`]). A file whose reference walk stopped
/// at a write that this repository does not hold
/// ([`release::FixReference::Unread`]) is carried after the fetch of that
/// write, where one of `remotes` has a branch or tag at it, and by no commit
/// here otherwise; and no commit carries a file that has no reference at all
/// ([`release::FixReference::Conflicted`]) until an adopt gives it one.
fn fix_remedy<'r>(fix: &'r release::LackedFix, remotes: &'r Remotes<'_>) -> FixRemedy<'r> {
    let (mut commit, mut fetches) = (fix.files.is_empty(), Vec::new());
    for file in &fix.files {
        let referenced = match &file.reference {
            release::FixReference::Write(_) => true,
            release::FixReference::Conflicted => false,
            release::FixReference::Unread { commit: write, .. } => remotes
                .at(write)
                .map(|(remote, reference)| Fetch {
                    remote,
                    reference,
                    commit: write,
                })
                .is_some_and(|found| {
                    if !fetches.contains(&found) {
                        fetches.push(found);
                    }
                    true
                }),
        };
        commit |= file.replayed || referenced;
    }
    FixRemedy { commit, fetches }
}

/// The last line of a refusal ([`refusal_remedy`]): `outcome`, then
/// `remedies`, what the refused findings ask for, `or` the `--allow-drop`
/// that states each drop, given on the line after it
/// ([`drop_flags_line`]), where the refusal names a drop (`drops`). Where a
/// remote could not be read for a fetch, `unread` names it.
fn closing_line(outcome: &str, remedies: &[String], (unread, drops): (&[&str], bool)) -> String {
    const ALLOW: &str = "re-run with --allow-drop to state the drop is intended";
    let unread = if unread.is_empty() {
        String::new()
    } else {
        format!("; {} could not be read", unread.join(" and "))
    };
    match (remedies.is_empty(), drops) {
        (true, _) => format!("  {outcome}; {ALLOW}{unread}"),
        (false, true) => format!("  {outcome}; {}, or {ALLOW}{unread}", remedies.join("; ")),
        (false, false) => format!("  {outcome}; {}{unread}", remedies.join("; ")),
    }
}

/// What a refused record asks for ([`refusal_remedy`]) about the members
/// that `sources` hold, the newest write where `absent` names it, and
/// `asks`, what each fix by hand that it lacks asks for, after each fetch
/// that they ask for ([`unread_fetch`]). It asks for a commit that carries
/// a fix only where a commit can carry it ([`FixRemedy::commit`]), so
/// `None` where nothing else is lacked: the adopt of the new content, or
/// `--allow-drop`, is then the way on.
fn record_remedy(
    (sources, absent, asks): (&[HeldRecord<'_>], Option<HeldRecord<'_>>, &[FixRemedy<'_>]),
) -> Option<String> {
    let holdable = asks.iter().filter(|ask| ask.commit).count();
    let remedy = match absent {
        Some(write) => format!(
            "fetch, then record a commit that carries what {} carries; {}",
            inline_human_text(write.recorded().commit.short()),
            adopt_remedy(write)
        ),
        None if sources.is_empty() && holdable == 0 => return None,
        None if sources.is_empty() && holdable == 1 => {
            "record a commit that carries that fix".to_owned()
        }
        None if sources.is_empty() => "record a commit that carries those fixes".to_owned(),
        None if holdable > 0 => "record a commit that carries each of them and each fix".to_owned(),
        None => "record a commit that carries each of them".to_owned(),
    };
    Some(format!("{}{remedy}", unread_fetch(asks)))
}

/// What a refusal says first where a fix needs a commit that this
/// repository does not hold and origin or the publish remote has: each
/// fetch that `asks`, what each fix asks for ([`fix_remedy`]), names: the
/// fetch of the write where the walk for the reference of a file that the
/// subject lacks stopped, a recorded write that this repository does not
/// hold ([`release::FixReference::Unread`]). No commit carries such a file
/// until this repository holds that write. Each names the remote and the
/// branch or tag that has the commit now, once: `fetch REMOTE's REF, which
/// holds COMMIT, then `. Empty where no fix asks for a fetch. The ref is
/// the remote's and the commit a ledger entry's evidence, so both are
/// escaped.
fn unread_fetch(asks: &[FixRemedy<'_>]) -> String {
    let mut fetches: Vec<Fetch<'_>> = Vec::new();
    for fetch in asks.iter().flat_map(|ask| &ask.fetches) {
        if !fetches
            .iter()
            .any(|seen| (seen.remote, seen.reference) == (fetch.remote, fetch.reference))
        {
            fetches.push(*fetch);
        }
    }
    if fetches.is_empty() {
        return String::new();
    }
    let named: Vec<String> = fetches
        .iter()
        .map(|fetch| {
            format!(
                "{}'s {}, which holds {}",
                inline_human_text(fetch.remote),
                inline_human_text(fetch.reference),
                inline_human_text(fetch.commit.short())
            )
        })
        .collect();
    format!("fetch {}, then ", named.join(" and "))
}

/// The fork's remotes, read at most once, where a refusal first asks where
/// a commit that this repository does not hold can be fetched from
/// ([`fix_remedy`]): origin, and the publish remote where it is another
/// remote. Each is read with one `git ls-remote` of its branches and tags
/// ([`knives::jj::remote_refs`]), so what a remedy names is what the remote
/// has now, not what this repository last fetched. The cut lets its lock go
/// before the first read ([`CutLock`]): only a refusal reads them.
struct Remotes<'a> {
    /// The remotes to read, each by name with its URL.
    urls: Vec<(&'a str, &'a str)>,
    /// What they have, once read.
    read: OnceCell<Advertised<'a>>,
    /// The cut's lock, let go before the first read; `None` in a test.
    lock: Option<&'a CutLock>,
}

impl<'a> Remotes<'a> {
    /// The remotes of `entry`, not read yet, for a cut that holds `lock`.
    fn of(entry: &'a knives::config::RepoEntry, lock: &'a CutLock) -> Self {
        let mut urls = vec![("origin", entry.remote(Role::Origin))];
        if entry.has_split_release() {
            urls.push((entry.publish_remote(), entry.remote(Role::Release)));
        }
        Self {
            urls,
            read: OnceCell::new(),
            lock: Some(lock),
        }
    }

    /// Remotes that have `advertised`, read already.
    #[cfg(test)]
    fn read_as(advertised: Advertised<'a>) -> Self {
        Self {
            urls: Vec::new(),
            read: OnceCell::from(advertised),
            lock: None,
        }
    }

    /// What the remotes have, read on the first call, once the cut has let
    /// its lock go.
    fn advertised(&self) -> &Advertised<'a> {
        self.read.get_or_init(|| {
            if let Some(lock) = self.lock {
                lock.release();
            }
            let mut advertised = Advertised::default();
            for &(name, url) in &self.urls {
                match knives::jj::remote_refs(url, &["refs/heads/*", "refs/tags/*"]) {
                    Ok(refs) => advertised.refs.push((name, refs)),
                    Err(_) => advertised.unread.push(name),
                }
            }
            advertised
        })
    }

    /// The remote and the branch or tag that has `commit` now
    /// ([`Advertised::at`]), reading the remotes if they are not read yet.
    fn at(&self, commit: &CommitId) -> Option<(&str, &str)> {
        self.advertised().at(commit)
    }

    /// The remotes that could not be read, where they were read: empty
    /// where no refusal asked.
    fn unread(&self) -> &[&'a str] {
        match self.read.get() {
            Some(advertised) => &advertised.unread,
            None => &[],
        }
    }
}

/// What the fork's remotes have now ([`Remotes::advertised`]).
#[derive(Debug, Default)]
struct Advertised<'a> {
    /// Each remote that was read, by name, with each of its branches and
    /// tags and that ref's commit.
    refs: Vec<(&'a str, BTreeMap<String, CommitId>)>,
    /// Each remote that could not be read, by name.
    unread: Vec<&'a str>,
}

impl Advertised<'_> {
    /// The first remote, in the order read, and the first of its branches
    /// and tags, in name order, whose commit is `commit`. A ledger may give
    /// the commit short, so a ref's full commit is `commit` where it starts
    /// with it, as the audit compares a recorded commit with a commit it
    /// reads; an empty commit names none. A peeled tag's `^{}` is left out.
    fn at(&self, commit: &CommitId) -> Option<(&str, &str)> {
        let recorded = commit.as_str();
        if recorded.is_empty() {
            return None;
        }
        self.refs.iter().find_map(|(remote, refs)| {
            refs.iter()
                .find(|(_, at)| at.as_str().starts_with(recorded))
                .map(|(reference, _)| (*remote, reference.strip_suffix("^{}").unwrap_or(reference)))
        })
    }
}

/// What a refusal over a fix by hand that the subject lacks says to keep
/// the subject's own content at the fix's files instead: change those files
/// by hand on the release of `write`, the newest recorded write, and run
/// `knives release adopt` on it, so that the new adopt holds them. The
/// release is a ledger entry's subject: shown escaped, and quoted for the
/// shell inside the command.
fn fix_adopt_remedy(write: HeldRecord<'_>) -> String {
    let name = &write.recorded().name;
    format!(
        "to keep the new content of those files, edit {} by hand to hold it and run `knives \
         release adopt {}`",
        inline_human_text(name),
        shell_word(name)
    )
}

/// What a refusal over `write`, a newest write this repository does not
/// hold, says after its fetch: where the fetch does not bring that commit,
/// as when git refused the push of a write that `adopt` had recorded, that
/// `knives release adopt <release>` records the commit this repository
/// holds as the release's newest write. The release and the commit are a
/// ledger entry's subject and first evidence string: shown escaped, and
/// the release quoted for the shell inside the command.
fn adopt_remedy(write: HeldRecord<'_>) -> String {
    let recorded = write.recorded();
    let (name, commit) = (&recorded.name, recorded.commit.short());
    format!(
        "if the fetch does not bring {}, `knives release adopt {}` records the commit this \
         repository holds as {}'s newest write",
        inline_human_text(commit),
        shell_word(name),
        inline_human_text(name)
    )
}

/// The lines of [`uncarried_cut_lines`] that name `dropped`, members that
/// `source`, one of `held`, names and the cut subject does not carry: one
/// statement under `--allow-drop`, else a refusal's heading and a line for
/// each member.
///
/// A cut names a member of the previous cut as it always has, and a member
/// only the newest write holds with that release and its write. A record
/// names a member of the newest write with that release, as the newest the
/// ledger records in the series, and a member only the previous cut holds
/// with that cut ([`HeldRecords::holds`]).
fn dropped_lines(
    repo: &RepoName,
    (held, source): (HeldRecords<'_>, HeldRecord<'_>),
    dropped: &[&str],
    allowed: bool,
) -> Vec<String> {
    let count = dropped.len();
    let name = inline_human_text(&source.recorded().name);
    let dropped: Vec<String> = dropped
        .iter()
        .map(|member| inline_human_text(member))
        .collect();
    let holds = held.holds(source, &name);
    if allowed {
        return vec![format!(
            "{repo}: --allow-drop: {} without {count} member(s) {holds}: {}",
            held.doing(),
            dropped.join(", ")
        )];
    }
    let heading = match (source, held.record) {
        (HeldRecord::PreviousCut(_), None) => format!(
            "{repo}: refusing to cut: the {} recorded {count} member(s) this cut does not carry:",
            source.title(&name)
        ),
        _ => format!(
            "{} does not carry {count} member(s) {holds}:",
            held.refusing(repo)
        ),
    };
    std::iter::once(heading)
        .chain(dropped.iter().map(|member| format!("    {member}")))
        .collect()
}

/// What a cut that recorded the published composition says: that, and the
/// previous recorded cut it follows. The previous cut's name and commit are a
/// ledger entry's subject and first evidence string, so each is escaped.
fn recorded_published_line(
    repo: &RepoName,
    (name, publish_remote): (&str, &str),
    published: &knives::ids::CommitId,
    previous: Option<&RecordedCut>,
) -> String {
    let previous = previous.map_or_else(
        || "no previous recorded cut".to_owned(),
        |recorded| {
            format!(
                "previous recorded cut {} was {}",
                inline_human_text(&recorded.name),
                inline_human_text(recorded.commit.short())
            )
        },
    );
    format!(
        "{repo}: recorded published composition {name}@{publish_remote} ({}) as the cut; \
         {previous}",
        published.short()
    )
}

struct CompletedCut<'a> {
    name: &'a str,
    request: &'a release::Cut,
    created: &'a knives::ids::CommitId,
    /// The parents of `created` that are earlier writes of its name, never
    /// members: a fixed branch's published commit that a cut records may
    /// have the release it followed as a parent. The event records each with
    /// the carrier label ([`knives::release_model::carrier_label`]).
    carriers: &'a [CommitId],
    audit: &'a release::CutAudit,
    /// Each member the audit read missing that `--allow-drop` let the
    /// record go ahead without ([`waived_members`]): the event says the
    /// commit is recorded without those members' lines.
    waived: &'a [(String, CommitId)],
    /// The members of the newest recorded write that a fixed branch's
    /// published commit holds only through a carrier, not as parents: the
    /// event keeps each in its evidence, so the next record is held to it
    /// as the gate held this one, and a later loss of its lines is named.
    through_carriers: &'a [CommitId],
    scheme: &'a ReleaseScheme,
    /// The records the cut was held against ([`CompositionGate::held`]), for
    /// its event to restate.
    held: Option<HeldRecords<'a>>,
    check: &'a release::CompositionCheck,
    /// What `--allow-drop` let the cut go ahead without, said after the reap.
    stated_drop: Option<&'a OrphanedLineage>,
    /// The release logs the cut wrote as the union of their sides.
    unions: &'a [knives::jj::UnionedFile],
    /// The mark the event ends with when knives could not check the release
    /// the cut duplicates ([`release::unchecked_suffix`]); empty otherwise.
    unchecked: &'a str,
    /// The composition of a release composed outside knives that `cut NAME
    /// --at` records: its members, every parent the trunk holds left out
    /// ([`record_hand_cut`]). The event records it, and says the release was
    /// composed outside knives ([`RECORDED_OUTSIDE`]). `None` for a cut
    /// knives wrote or recorded as published, whose event reads its
    /// composition from the commit ([`release::parent_sources`]).
    composition: Option<&'a [(String, CommitId)]>,
}

impl CompletedCut<'_> {
    /// The exit of the cut's checks: findings where the audit or the
    /// composition gate could not read a member, or a fix by hand, for an
    /// unresolved conflict; else 0.
    const fn exit(&self) -> Exit {
        if self.audit.inconclusive.is_empty() && !self.check.is_inconclusive() {
            Exit::Ok
        } else {
            Exit::Findings
        }
    }
}

/// What the event of a cut composed outside knives says after it restates
/// the records it was held against ([`restated_delta`]): `cut NAME --at`
/// recorded it, knives wrote no commit.
const RECORDED_OUTSIDE: &str = "; recorded: composed outside knives";

/// Record which branches and commits became a published release cut.
///
/// The text names the cut's change id beside its commit id. Resolving the
/// cut's conflicts before pushing rewrites the merge, so the commit the event
/// names is not the commit the release remote ends up holding, while the
/// change id survives the
/// rewrite and still resolves to the release. Evidence keeps the commit first:
/// the composition gate parses it from that position. The release logs the
/// cut unioned close the text, after everything a reader of the event parses,
/// and a cut composed outside knives says so before them. The parents
/// recorded are the cut's composition ([`CompletedCut::composition`], else
/// [`release::parent_sources`]).
fn record_cut_event(
    fork: &Fork<'_>,
    cut: &CompletedCut<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    let entry = fork.entry;
    let opened = knives::jj::Repo::open(&fork.checkout.path)?;
    let change = opened.change_id_of(cut.created.as_str())?;
    let members: std::borrow::Cow<'_, [(String, CommitId)]> =
        if let Some(composition) = cut.composition {
            std::borrow::Cow::Borrowed(composition)
        } else {
            let parents: Vec<CommitId> = opened
                .parent_commits(cut.created.as_str())?
                .into_iter()
                .filter(|parent| !cut.carriers.contains(parent))
                .collect();
            std::borrow::Cow::Owned(release::parent_sources(
                &opened, entry, cut.scheme, &parents,
            )?)
        };
    let members_text = members_event_text(&members);
    let carried_writes = if cut.carriers.is_empty() {
        String::new()
    } else {
        let shorts: Vec<&str> = cut.carriers.iter().map(CommitId::short).collect();
        format!(
            "; carries {}'s earlier write(s), not members: {}",
            cut.name,
            shorts.join(", ")
        )
    };
    let parents: Vec<CommitId> = members.iter().map(|(_, commit)| commit.clone()).collect();
    let mut evidence = vec![cut.created.as_str().to_owned()];
    evidence.extend(parents.iter().map(|commit| commit.as_str().to_owned()));
    // An unverified member stays in evidence so the next gate rechecks it:
    // dropping it from the baseline here would let one conflicted cut launder
    // a member out of the composition without anyone ever stating the drop.
    let unverified: Vec<String> = cut
        .check
        .inconclusive
        .iter()
        .map(|member| member.commit.as_str().to_owned())
        .filter(|sha| !evidence.contains(sha))
        .collect();
    evidence.extend(unverified);
    let kept_text = keep_members_held_through_carriers(cut.through_carriers, &mut evidence);
    let delta = restated_delta(cut.held, cut.check);
    let mut recorded = recorded_parents(&opened, entry, &parents)?;
    recorded.extend(
        cut.carriers
            .iter()
            .map(|carrier| knives::ledger::RecordedParent {
                commit: carrier.as_str().to_owned(),
                branches: vec![knives::release_model::carrier_label(cut.name)],
            }),
    );
    let waived = waived_event_text(cut.waived);
    scribe_for(fork, bound)?.record(&Draft {
        evidence,
        parents: recorded,
        ..Draft::event(
            Some(cut.name),
            format!(
                "cut {} as {} (change {}) with {} parent(s): \
                 {members_text}{carried_writes}{kept_text}{delta}{waived}{}{}{}",
                cut.name,
                cut.created.short(),
                change.short(),
                members.len(),
                if cut.composition.is_some() {
                    RECORDED_OUTSIDE
                } else {
                    ""
                },
                union_event_text(cut.unions),
                cut.unchecked
            ),
            None,
        )
    })?;
    Ok(())
}

/// Add to `evidence` each of `held`'s members it does not hold yet, and the
/// event text that names them ([`CompletedCut::through_carriers`]). A member
/// the commit holds through a carrier stays a recorded member, as an edit
/// event keeps every member: a record that listed only the commit's parents
/// would drop it from every later gate.
fn keep_members_held_through_carriers(held: &[CommitId], evidence: &mut Vec<String>) -> String {
    let kept: Vec<&CommitId> = held
        .iter()
        .filter(|member| !evidence.iter().any(|sha| sha == member.as_str()))
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    evidence.extend(kept.iter().map(|member| member.as_str().to_owned()));
    let shorts: Vec<&str> = kept.iter().map(|member| member.short()).collect();
    format!(
        "; holds {} member(s) of its newest recorded write through its earlier write(s): {}",
        shorts.len(),
        shorts.join(", ")
    )
}

/// The event text naming each member `--allow-drop` let the record go ahead
/// without ([`CompletedCut::waived`]); empty where there is none.
fn waived_event_text(waived: &[(String, CommitId)]) -> String {
    if waived.is_empty() {
        return String::new();
    }
    let named: Vec<String> = waived
        .iter()
        .map(|(label, commit)| format!("{label}@{}", commit.short()))
        .collect();
    format!(
        "; without the lines of {} member(s) that it lacks: {}",
        named.len(),
        named.join(", ")
    )
}

/// What a cut event says after its members about the records the cut was
/// held against ([`CompositionGate::held`]): the first record as what it is
/// and how many members it holds ([`HeldRecord::restated`]), `; previous cut
/// NAME recorded N member(s)` or `; NAME's newest recorded write (an edit)
/// holds N member(s)`, then `all carried` or the members the subject
/// dropped, each member that only the other record holds named with that
/// record, then `; not held against <the newest write> <commit>, which this
/// repository does not hold` for a subject that `--allow-drop` let go ahead
/// without that write, then `; without the fix by hand of <release>'s adopt
/// <commit>, in <n> file(s): <files>` for each fix by hand that
/// `--allow-drop` let the subject go ahead without, with the files where it
/// lacks it ([`fix_title`]), then `; not held against N adopt(s) this
/// repository does not hold: <commits>`, then the unverified members and
/// each fix by hand left inconclusive. Empty when the ledger records
/// neither. The event keeps each name and path raw.
fn restated_delta(held: Option<HeldRecords<'_>>, check: &release::CompositionCheck) -> String {
    let Some(held) = held else {
        return String::new();
    };
    let (first, other) = held.split(&check.dropped);
    let mut dropped = Vec::new();
    if !first.is_empty() {
        dropped.push(format!("dropped: {}", first.join(", ")));
    }
    if let Some((other, names)) = other
        && !names.is_empty()
    {
        dropped.push(format!(
            "dropped from {}: {}",
            other.title(&other.recorded().name),
            names.join(", ")
        ));
    }
    let carried = if dropped.is_empty() {
        "all carried".to_owned()
    } else {
        dropped.join("; ")
    };
    let absent = held
        .newest_write()
        .filter(|_| check.absent_write)
        .map(|write| {
            format!(
                "; not held against {}, which this repository does not hold",
                write.with_commit(str::to_owned)
            )
        })
        .unwrap_or_default();
    let fixes: Vec<String> = check
        .dropped_fixes
        .iter()
        .map(|fix| {
            format!(
                "; without {}{}",
                fix_title((&fix.name, &fix.commit), str::to_owned),
                fix_files(&fix.files, str::to_owned)
            )
        })
        .collect();
    let fixes = fixes.concat();
    let absent_adopts = if check.absent_adopts.is_empty() {
        String::new()
    } else {
        let shorts: Vec<&str> = check
            .absent_adopts
            .iter()
            .map(|adopt| adopt.commit.short())
            .collect();
        format!(
            "; not held against {} adopt(s) this repository does not hold: {}",
            shorts.len(),
            shorts.join(", ")
        )
    };
    let unverified: Vec<String> = check
        .inconclusive
        .iter()
        .map(|member| member.name.clone())
        .chain(check.inconclusive_fixes.iter().map(|fix| {
            format!(
                "{}{}",
                fix_title((&fix.name, &fix.commit), str::to_owned),
                fix_files(&fix.files, str::to_owned)
            )
        }))
        .collect();
    let unverified = if unverified.is_empty() {
        String::new()
    } else {
        format!("; unverified: {}", unverified.join(", "))
    };
    format!(
        "; {}; {carried}{absent}{fixes}{absent_adopts}{unverified}",
        held.first.restated()
    )
}

/// A commit knives did not write, held as every cut is held before it is
/// recorded: the fixed branch's published composition ([`run_release`]) and
/// a release cut outside knives ([`record_hand_cut`]).
struct CommittedCut<'a> {
    commit: &'a CommitId,
    /// Each member with where it came from, as the audit reads them.
    members: &'a [(String, CommitId)],
    /// What the audit measures each member and the drift from
    /// ([`release::audit_cut`]).
    context: release::AuditContext<'a>,
    carried_from: release::CarriedFrom,
    /// Whether `--allow-drop` may name a member the audit reads missing, so
    /// that the commit is recorded without the lines it lacks of that
    /// member ([`waived_members`]): the fixed branch's published commit,
    /// which is published already and has no other name to be made again
    /// under. A release cut outside knives is refused instead, to be made
    /// again.
    waivable: bool,
}

/// What [`checked_committed_cut`] found, for the cut event to restate.
struct CheckedCut {
    audit: release::CutAudit,
    check: release::CompositionCheck,
    /// Each member the audit read missing that `--allow-drop` named, by its
    /// label and commit: the event records the commit without those lines.
    waived: Vec<(String, CommitId)>,
}

/// Audit `cut`'s commit ([`report_cut_audit`]), then hold it against the
/// gate's baseline ([`recorded_composition_check`]). `Err` is the exit of a
/// refusal, already reported: nothing is recorded. A file of a fix by hand
/// that the gate holds is the gate's to judge, not the drift check's
/// ([`release::CutAudit::leave_fixes_to_gate`]). Where the commit is
/// `waivable`, a member the audit reads missing is a drop that
/// `--allow-drop` names by its commit ([`waived_members`]), and a refusal
/// over such members ends with the flag of each.
fn checked_committed_cut(
    repo: &RepoName,
    cut: &CommittedCut<'_>,
    (gate, allow_drop): (&CompositionGate<'_>, &AllowDrop),
) -> anyhow::Result<Result<CheckedCut, Exit>> {
    let mut audit = release::audit_cut(
        gate.path,
        cut.members,
        release::CutSubject::Committed(cut.commit),
        cut.context,
    )?;
    audit.leave_fixes_to_gate(&gate.fixes);
    let (waived, drops) = if cut.waivable {
        waived_members(repo, &mut audit, allow_drop)
    } else {
        (Vec::new(), Vec::new())
    };
    if let Some(exit) = report_cut_audit(
        repo,
        &audit,
        (cut.carried_from, cut.context.drift_from),
        &drops,
    ) {
        return Ok(Err(exit));
    }
    let mut subject = release::CutSubject::Committed(cut.commit);
    Ok(
        recorded_composition_check(repo, &mut subject, gate, allow_drop)?.map(|check| CheckedCut {
            audit,
            check,
            waived,
        }),
    )
}

/// The members of `audit` read missing that `--allow-drop` names, each
/// taken out of the audit's missing members, with the lines that state each
/// drop held for [`AllowDrop::settle`]; and, where a value does not name
/// every such member, none taken out and the name of each, short, for the
/// refusal's flags ([`drop_flags_line`]). Only members are waived: a file no
/// member explains refuses the commit whatever `--allow-drop` names.
fn waived_members(
    repo: &RepoName,
    audit: &mut release::CutAudit,
    allow_drop: &AllowDrop,
) -> (Vec<(String, CommitId)>, Vec<String>) {
    let missing: Vec<(String, CommitId)> = audit
        .missing
        .iter()
        .filter_map(|label| {
            audit
                .members
                .iter()
                .find(|lines| &lines.label == label)
                .map(|lines| (label.clone(), lines.commit.clone()))
        })
        .collect();
    if missing.is_empty() || missing.len() != audit.missing.len() || !audit.unexplained.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut every = true;
    for (_, commit) in &missing {
        every &= allow_drop.commit(commit);
    }
    if !every {
        let drops = missing
            .iter()
            .map(|(_, commit)| commit.short().to_owned())
            .collect();
        return (Vec::new(), drops);
    }
    let mut lines = Vec::new();
    for (label, _) in &missing {
        lines.push(format!(
            "{repo}: --allow-drop: recording without the lines of {} that the published commit \
             lacks:",
            inline_human_text(label)
        ));
        lines.extend(audit.lost_lines_of(label));
    }
    allow_drop.state(lines);
    audit.missing.clear();
    (missing, Vec::new())
}

/// `knives release cut NAME --at REV`: record the commit `at` names, a
/// release cut outside knives, as `name`'s cut. No commit is made, and no
/// orphan gate, reap, signing or push of the release runs.
///
/// The name and the pulled plan are gated before this ([`requested_cut`],
/// [`release_plan_exit`]), and the commit and its name here
/// ([`hand_cut_gates`]). Its members are its parents, its carriers and
/// every parent the trunk holds left out ([`hand_cut_composition`]),
/// audited as every cut is ([`checked_committed_cut`]). The audit reads
/// each member from its own fork point with the upstream trunk, and each
/// parent the trunk holds from the fork point of all the members and the
/// trunk ([`HandCut::base`]), and measures the drift
/// from the release the record follows: `follows`, the newest other release
/// of the series, as a cut's audit measures it
/// ([`previous_release_for_record`]). A first record follows no release,
/// and its drift is measured from its members' fork point with the
/// trunk: it is the release every later record's drift is measured from, so
/// a change no member explains is refused in it too. Then the commit is held
/// against the gate's baseline ([`CompositionBaseline`], NAME aside): the
/// members of the series' previous cut and of the newest write of the newest
/// release the ledger records in the series. So a member `include` added
/// after that release's cut is held too, and so is each fix by hand that an
/// `adopt` of any release of the series recorded, file by file, at each
/// file where its hold has not ended ([`release::adopted_fixes`]): the
/// commit's own tree must carry the fix there, by a replay of the adopt's
/// change. An adopt this repository does not hold holds nothing, and is
/// named. The drift check leaves those files to the gate. This repository
/// must hold the newest write, or the record is refused and told to fetch
/// it ([`release::CompositionCheck::absent_write`]). A drop goes ahead only
/// where `--allow-drop` names it ([`AllowDrop`]). Each refusal writes
/// nothing. Only then is the local bookmark made, where no ref of the name
/// exists, and the event recorded, which the process sends to a shared
/// ledger before it exits; the commit's repeated headings print once after
/// them, even where the record fails after the bookmark was made
/// ([`write_hand_cut`]).
fn record_hand_cut(
    (fork, opened, lock): (&Fork<'_>, &knives::jj::Repo, &CutLock),
    (name, at, allow_drop): (&str, &str, &AllowDrop),
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let (repo, entry) = (&fork.name, fork.entry);
    // The pulled plan refused on any problem this read finds.
    let ledger = Ledger::for_fork(fork).read()?.entries;
    let tips = opened.bookmark_tips()?;
    let trunk = opened.resolve_commit(&entry.trunk_ref())?;
    let follows = previous_release_for_record(entry, &tips, name);
    let held = match hand_cut_gates(
        (fork, opened),
        (name, at),
        (&ledger, &tips, follows.as_ref()),
        &trunk,
    )? {
        Ok(held) => held,
        Err(exit) => return Ok(exit),
    };
    let commit = &held.commit;
    let writes = EarlierWrites::read(opened, name, &ledger, held.published.as_ref())?;
    let composed = match hand_cut_composition(
        (fork, opened),
        (name, commit),
        &writes,
        (&tips, &trunk, follows.as_ref()),
    )? {
        Ok(composed) => composed,
        Err(exit) => return Ok(exit),
    };
    let parents = opened.parent_commits(commit.as_str())?;
    let record = Some((name, commit));
    let (baseline, fixes) =
        CompositionGate::read_baseline((opened, entry), (&ledger, &tips), record)?;
    let gate = CompositionGate {
        baseline,
        record,
        opened,
        entry,
        lock,
        ledger: &ledger,
        path: &fork.checkout.path,
        parents: &parents,
        base: &composed.base,
        trunk: &trunk,
        tips: &tips,
        fixes,
    };
    let committed = CommittedCut {
        commit,
        members: &composed.members,
        // Nothing is carried from an earlier release: a member this commit
        // lacks is missing, however the commit came to drop it. A parent the
        // trunk holds is no member, but the release holds its content: it is
        // read from the members' fork point, and its files explain drift, as
        // a member's.
        context: release::AuditContext {
            previous: None,
            forward_members: None,
            upstream_trunk: &trunk,
            trunk_parents: &composed.bases,
            trunk_parent_base: Some(&composed.base),
            drift_trunk: &composed.base,
            drift_from: Some(composed.drift_from(follows.as_ref().map(|(_, previous)| previous))),
        },
        carried_from: release::CarriedFrom::Unledgered,
        waivable: false,
    };
    let checked = match checked_committed_cut(repo, &committed, (&gate, allow_drop))? {
        Ok(checked) => checked,
        Err(exit) => return Ok(exit),
    };
    if let Some(exit) = allow_drop.settle(repo) {
        return Ok(exit);
    }
    // The event records the members, never a parent the trunk holds.
    let cut = CompletedCut {
        name,
        request: &release::Cut::from_carried(name.to_owned(), &composed.members),
        created: commit,
        carriers: &[],
        audit: &checked.audit,
        scheme: &entry.release_scheme(),
        held: gate.held(),
        check: &checked.check,
        stated_drop: None,
        waived: &[],
        through_carriers: &[],
        // Recorded, not written: knives unioned nothing.
        unions: &[],
        unchecked: "",
        composition: Some(&composed.members),
    };
    let unpushed = held.published.is_none();
    release::with_last_write_headings(|last| {
        write_hand_cut(fork, &cut, (held.named, unpushed), (bound, last))
    })
}

/// The writes of `cut NAME --at REV` once every gate passed: the local
/// bookmark at the commit where no ref of the name exists (`named` false),
/// then the event; then the line that says so, with the push where the
/// publish remote does not hold the name (`unpushed`). The first of those
/// writes is read into `last` as soon as it is done
/// ([`release::ReportedWrite`]), so the commit's repeated headings print
/// once after the steps, even where the record after the bookmark fails.
fn write_hand_cut(
    fork: &Fork<'_>,
    cut: &CompletedCut<'_>,
    (named, unpushed): (bool, bool),
    (bound, last): (Bound<'_>, &mut Option<release::ReportedWrite>),
) -> anyhow::Result<Exit> {
    let path = &fork.checkout.path;
    let reported = || release::ReportedWrite::read(path, cut.created.clone());
    if !named {
        knives::jj::set_bookmark(path, cut.name, cut.created.as_str())?;
        *last = Some(reported());
    }
    record_cut_event(fork, cut, bound)?;
    // Where a ref of the name was there already, the record is the write.
    if last.is_none() {
        *last = Some(reported());
    }
    println!(
        "{}",
        recorded_hand_cut_line(
            &fork.name,
            (cut.name, cut.created),
            unpushed.then_some(fork.entry.publish_remote())
        )
    );
    // The commit holds no conflict, but a member whose tip is conflicted in
    // one of its own paths cannot be read there, as for any cut.
    Ok(cut.exit())
}

/// The commit a release cut outside knives is recorded at, as its gates
/// read it and its name ([`hand_cut_gates`]).
struct HandCutAt {
    commit: CommitId,
    /// Whether a ref of the name exists here or on the publish remote: each
    /// names the commit. With none, the record makes the local bookmark.
    named: bool,
    /// The publish remote's commit of the name: the commit, or none.
    published: Option<CommitId>,
}

/// The gates of `cut NAME --at REV` after the name's and the plan's, in this
/// order, each refusal writing nothing and exiting 3: REV is one commit that
/// shares history with `trunk` ([`knives::jj::Repo::shares_history`]: the
/// root commit, or a commit made on it apart from the trunk, holds none of
/// the series' base), finished
/// ([`super::release_adopt::refuse_unfinished_commit`]); a recorded write of
/// the name is REV, which is already the record (exit 0), or `adopt`'s to
/// replace ([`refuse_recorded_name`]); the name sorts after every other
/// release of the series ([`newer_release`], with `follows`, the newest
/// other release here or on the publish remote); each ref of the name names
/// REV ([`name_refs`]); a published name is one the publish remote holds,
/// else the record waits for its push ([`unpushed_record_remedy`]). `Err`
/// is the exit, after saying why.
fn hand_cut_gates(
    (fork, opened): (&Fork<'_>, &knives::jj::Repo),
    (name, at): (&str, &str),
    (ledger, tips, follows): (
        &[knives::ledger::Entry],
        &knives::detect::BookmarkTips,
        Option<&(BookmarkRef, CommitId)>,
    ),
    trunk: &CommitId,
) -> anyhow::Result<Result<HandCutAt, Exit>> {
    let (repo, entry) = (&fork.name, fork.entry);
    let shown = inline_human_text(name);
    let commit = match opened.resolve_commit(at) {
        Ok(commit) => commit,
        Err(error) => {
            println!(
                "{repo}: refusing to record {shown}: {} does not resolve to one commit ({})",
                inline_human_text(at),
                problem_human_text(&error.to_string())
            );
            return Ok(Err(Exit::Incomplete));
        }
    };
    if !opened.shares_history(&commit, trunk)? {
        // The trunk is a registry value, like every other, and not escaped.
        println!(
            "{repo}: refusing to record {shown} at {}: it shares no history with the trunk {}: \
             no commit but the root is an ancestor of both, so it is no release of the series",
            commit.short(),
            entry.trunk_ref()
        );
        return Ok(Err(Exit::Incomplete));
    }
    if super::release_adopt::refuse_unfinished_commit(
        repo,
        opened,
        (name, &commit),
        ("record it", "recorded"),
    )? {
        return Ok(Err(Exit::Incomplete));
    }
    if let Some(exit) = refuse_recorded_name(repo, (name, &commit), ledger) {
        return Ok(Err(exit));
    }
    let (scheme, publish_remote) = (entry.release_scheme(), entry.publish_remote());
    if let Some((newer, held)) = newer_release(&scheme, follows, ledger, name) {
        println!(
            "{repo}: refusing to record {shown}: {} sorts after it, and {held}; a record takes a \
             name that sorts after every other release of the series, so record each release in \
             order, the older name first",
            inline_human_text(&newer)
        );
        return Ok(Err(Exit::Incomplete));
    }
    let refs = name_refs(opened, (tips, &scheme, publish_remote), name)?;
    if refs
        .iter()
        .any(|(_, targets)| targets.as_slice() != std::slice::from_ref(&commit))
    {
        let listed: Vec<String> = refs
            .iter()
            .map(|(reference, targets)| {
                let targets: Vec<&str> = targets.iter().map(CommitId::short).collect();
                format!("{reference} at {}", targets.join(" and "))
            })
            .collect();
        println!(
            "{repo}: refusing to record {shown} at {}: each ref of it must name that commit, and \
             its refs are {}; record the commit they name, or move them first",
            commit.short(),
            listed.join(", ")
        );
        return Ok(Err(Exit::Incomplete));
    }
    let published = tips.get(&BookmarkRef::Remote {
        branch: BranchName::new(name),
        remote: RemoteName::new(publish_remote),
    });
    // Each ref of the name names REV now, so the publish remote holds REV
    // exactly when it holds the name.
    if published.is_none()
        && let Some(evidence) = release::published_release(
            opened,
            ledger,
            release::ReleaseAt {
                scheme: &scheme,
                name,
                commits: &[Some(&commit)],
            },
        )?
    {
        println!(
            "{repo}: {}",
            unpushed_record_remedy(fork, (name, &commit), (&evidence, !refs.is_empty()))
        );
        return Ok(Err(Exit::Incomplete));
    }
    Ok(Ok(HandCutAt {
        published: published.cloned(),
        named: !refs.is_empty(),
        commit,
    }))
}

/// Why `name` at `commit`, which `evidence` marks published (a tag or a
/// `record: published` note names the commit), is not recorded while the
/// publish remote does not hold the name, and the commands that record it.
///
/// Consumers are given what the publish remote holds, so the record waits
/// for the push of the name at the commit. The commands make the local
/// bookmark first where no ref of the name exists (`named` is false), push
/// it, then run the same record again.
fn unpushed_record_remedy(
    fork: &Fork<'_>,
    (name, commit): (&str, &CommitId),
    (evidence, named): (&str, bool),
) -> String {
    let (repo, entry) = (&fork.name, fork.entry);
    let remote = entry.publish_remote();
    // The name is the caller's, kept to the series' grammar, whose label
    // takes any character but `/`: shown escaped, and quoted for the shell
    // inside each command. The evidence names a tag or quotes a ledger note.
    let (shown, word, at) = (inline_human_text(name), shell_word(name), commit.short());
    let series = entry
        .selected_series
        .as_ref()
        .map_or_else(String::new, |series| format!(" --series {series}"));
    let mut steps = Vec::new();
    if !named {
        steps.push(format!("`jj bookmark create {word} -r {at}`"));
    }
    steps.push(format!("`jj git push --remote {remote} --bookmark {word}`"));
    steps.push(format!(
        "`knives release --repo {repo}{series} cut {word} --at {at}`"
    ));
    format!(
        "refusing to record {shown} at {at}: it is published ({}), and {remote} does not hold \
         {shown}; a published release is recorded once the publish remote holds it: {}",
        inline_human_text(evidence),
        steps.join(", then ")
    )
}

/// What `cut NAME --at` says once it recorded `name` at `commit`, with the
/// push to make when `push_to`, the publish remote, does not hold the name.
fn recorded_hand_cut_line(
    repo: &RepoName,
    (name, commit): (&str, &CommitId),
    push_to: Option<&str>,
) -> String {
    // The name is the caller's, kept to the series' grammar, whose label
    // takes any character but `/`: shown escaped, and quoted for the shell
    // inside the command.
    let shown = inline_human_text(name);
    let push = push_to.map_or_else(String::new, |remote| {
        format!(
            "; push {shown} with jj git push --remote {remote} --bookmark {}",
            shell_word(name)
        )
    });
    format!(
        "{repo}: recorded {shown} at {} as its cut; nothing was committed{push}",
        commit.short()
    )
}

/// The refusal of a name the ledger already records a write of, unless the
/// newest such write is `commit`, which is already the record: `Some(Ok)`
/// after saying so. A commit made over a recorded write is `adopt`'s to
/// record, never a cut's.
fn refuse_recorded_name(
    repo: &RepoName,
    (name, commit): (&str, &CommitId),
    ledger: &[knives::ledger::Entry],
) -> Option<Exit> {
    let newest = knives::release_model::recorded_writes(ledger, name).next()?;
    let (shown, described) = (inline_human_text(name), release::described_write(&newest));
    if newest.commit == *commit {
        println!(
            "{repo}: {shown} at {} is already recorded, {described}; nothing to record",
            commit.short()
        );
        return Some(Exit::Ok);
    }
    println!(
        "{repo}: refusing to record {shown} at {}: the ledger already records {shown}, newest as \
         {described}; a commit made over a recorded write is recorded with `knives release adopt \
         {}`",
        commit.short(),
        shell_word(name)
    );
    Some(Exit::Incomplete)
}

/// The newest release of `scheme` other than `name` that sorts after it or
/// with it, and where it is held: `follows`, the newest other release here
/// or on the publish remote ([`previous_release_for_record`]), or a cut or
/// edit the ledger records
/// ([`knives::release_model::recorded_release_names`]).
///
/// A record takes a name that sorts after every other release of its series,
/// as a cut does: the reap after the next cut keeps only the newest name.
fn newer_release(
    scheme: &ReleaseScheme,
    follows: Option<&(BookmarkRef, CommitId)>,
    ledger: &[knives::ledger::Entry],
    name: &str,
) -> Option<(String, &'static str)> {
    let key = scheme.cut_key(name)?;
    let held = follows.map(|(reference, _)| {
        (
            reference.branch().as_str(),
            "a ref here or on the publish remote names it",
        )
    });
    let recorded = knives::release_model::recorded_release_names(ledger, scheme)
        .into_iter()
        .filter(|subject| *subject != name)
        .map(|subject| (subject, "the ledger records a cut or edit of it"));
    held.into_iter()
        .chain(recorded)
        .filter(|(newer, _)| scheme.cut_key(newer) >= Some(key))
        .max_by(|(left, _), (right, _)| scheme.cut_key(left).cmp(&scheme.cut_key(right)))
        .map(|(newer, held)| (newer.to_owned(), held))
}

/// Every ref of `name` here and on `publish_remote`, each with what it
/// names: its commit, or each target of a divergent ref. `name` is one of
/// `scheme`'s cuts ([`release::cut_name`]).
fn name_refs(
    opened: &knives::jj::Repo,
    (tips, scheme, publish_remote): (&knives::detect::BookmarkTips, &ReleaseScheme, &str),
    name: &str,
) -> anyhow::Result<Vec<(String, Vec<CommitId>)>> {
    let ours = |reference: &BookmarkRef| {
        reference.branch().as_str() == name
            && knives::ids::is_our_release(reference, scheme, publish_remote)
    };
    // Each is a ref read from the repository.
    let mut refs: Vec<(String, Vec<CommitId>)> = tips
        .iter()
        .filter(|(reference, _)| ours(reference))
        .map(|(reference, commit)| {
            (
                inline_human_text(&reference.to_string()),
                vec![commit.clone()],
            )
        })
        .collect();
    refs.extend(
        opened
            .conflicted_bookmarks()?
            .into_iter()
            .filter(|(reference, _)| ours(reference))
            .map(|(reference, targets)| (inline_human_text(&reference.to_string()), targets)),
    );
    Ok(refs)
}

/// What a release cut outside knives composes.
struct HandCut {
    /// Each member with where it came from ([`release::parent_sources`]).
    /// Never a parent the trunk holds.
    members: Vec<(String, CommitId)>,
    /// Each parent of the reading that the trunk holds, with where it came
    /// from: never a member ([`trunk_reaches`]), but content the release
    /// holds all the same.
    bases: Vec<(String, CommitId)>,
    /// The trunk's fork point with the members, as for a first cut: what
    /// the audit reads each of `bases` from, and the composition gate's
    /// base. With no release to follow, the series' first record measures
    /// its drift from it too ([`Self::drift_from`]).
    base: CommitId,
}

impl HandCut {
    /// What the record's drift is measured from: `follows`, the series'
    /// previous release. The series' first record follows no release, so its
    /// drift is measured from `base`, its members' fork point with the
    /// trunk, and a change no member explains is refused as in a later
    /// record.
    fn drift_from<'a>(
        &'a self,
        follows: Option<&'a CommitId>,
    ) -> (&'a CommitId, release::DriftBase) {
        follows.map_or((&self.base, release::DriftBase::ForkPoint), |previous| {
            (previous, release::DriftBase::PreviousRelease)
        })
    }
}

/// The members of `commit`, a release cut outside knives, or the exit after
/// refusing it.
///
/// A parent the trunk holds, which a trunk position reaches, is never a
/// member: it is one of the bases. Its members are its parents, its
/// carriers left out, and it is refused when it merges no member, when a
/// member is `follows`, the series' previous release, which is never a
/// member either ([`previous_parent_remedy`]), or when a member carries a
/// merge past the trunk ([`chain_record_remedy`]): a release is one flat
/// octopus merge ([`FLAT_RELEASE`]).
fn hand_cut_composition(
    (fork, opened): (&Fork<'_>, &knives::jj::Repo),
    (name, commit): (&str, &CommitId),
    writes: &EarlierWrites,
    (tips, trunk, follows): (
        &knives::detect::BookmarkTips,
        &CommitId,
        Option<&(BookmarkRef, CommitId)>,
    ),
) -> anyhow::Result<Result<HandCut, Exit>> {
    let (repo, entry) = (&fork.name, fork.entry);
    let (shown, at) = (inline_human_text(name), commit.short());
    let reading = read_release(opened, (writes, commit))?;
    let trunks = trunk_positions_in(tips, entry);
    let (mut members, mut bases) = (Vec::new(), Vec::new());
    for sourced in
        release::parent_sources(opened, entry, &entry.release_scheme(), &reading.members)?
    {
        if trunk_reaches(opened, &trunks, &sourced.1)? {
            bases.push(sourced);
        } else {
            members.push(sourced);
        }
    }
    if members.is_empty() {
        println!(
            "{repo}: refusing to record {shown} at {at}: it merges no member: a parent the trunk \
             holds is never a member, and it has no other parent; a release merges the series' \
             branches"
        );
        return Ok(Err(Exit::Incomplete));
    }
    if let Some((previous, previous_commit)) =
        follows.filter(|(_, previous)| members.iter().any(|(_, member)| member == previous))
    {
        // The previous release is a ref read from the repository.
        println!(
            "{repo}: refusing to record {shown} at {at}: its parent {} is {}, the series' \
             previous release, which is never a member; {}",
            previous_commit.short(),
            inline_human_text(previous.branch().as_str()),
            previous_parent_remedy()
        );
        return Ok(Err(Exit::Incomplete));
    }
    let stacked = stacked_branches(opened, entry, &members)?;
    if !stacked.is_empty() {
        for detail in &stacked {
            println!("{repo}: {detail}");
        }
        println!(
            "{repo}: refusing to record {shown} at {at}: {}",
            chain_record_remedy()
        );
        return Ok(Err(Exit::Incomplete));
    }
    let member_tips: Vec<CommitId> = members.iter().map(|(_, tip)| tip.clone()).collect();
    let base = opened.fork_point_of(&member_tips, trunk)?;
    Ok(Ok(HandCut {
        members,
        bases,
        base,
    }))
}

/// What every release is: a record of a commit built on an earlier release
/// is refused with it ([`previous_parent_remedy`], [`chain_record_remedy`]).
const FLAT_RELEASE: &str =
    "a release must be a flat octopus merge of the trunk and its member branches";

/// Why a release cut outside knives whose member carries a merge past the
/// trunk is refused, and how to make the release one flat octopus merge.
fn chain_record_remedy() -> String {
    format!(
        "a member above stacks on an earlier merge; {FLAT_RELEASE}: rebase each member above off \
         the trunk"
    )
}

/// Why a release cut outside knives that merges its series' previous
/// release as a member is refused, and how to make the release one flat
/// octopus merge.
fn previous_parent_remedy() -> String {
    format!("{FLAT_RELEASE}: merge the members, not the previous release")
}

/// What a published and recorded cut prints, and the exit of its checks
/// and its reap. `last` holds the cut's write, read as soon as the publish
/// was done ([`release::with_last_write_headings`]): the report takes it out
/// where it prints the write's conflicts and repeated headings, so those
/// headings print there once, whatever fails after.
fn report_completed_cut(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    cut: &CompletedCut<'_>,
    (bound, last): (Bound<'_>, &mut Option<release::ReportedWrite>),
) -> anyhow::Result<Exit> {
    let entry = fork.entry;
    let mut post_cut_exit = cut.exit();
    print_previous_release_position(opened, entry);
    println!(
        "  cut {} as {} with {} parent(s), not pushed",
        cut.name,
        cut.created.short(),
        cut.request.parents.len()
    );
    if let Some(written) = last.take() {
        println!(
            "{}",
            release::written_report(&written, cut.name, cut.unions)
        );
    }
    if let Some(first) = cut.request.parents.first() {
        let test_count = release::check_test_count(fork, cut.created, first);
        println!("{}", test_count.render());
        if matches!(test_count, release::TestCountCheck::Dropped { .. }) {
            post_cut_exit = post_cut_exit.worst(Exit::Findings);
        }
    }
    let present: Vec<String> = opened
        .workspaces()?
        .into_iter()
        .map(|(workspace, _)| workspace.to_string())
        .collect();
    // Only a workspace knives made for a branch that is gone everywhere and
    // that no live claim holds is left-behind cruft: any other is someone's.
    let recorded: Vec<String> = Ledger::for_fork(fork)
        .read()?
        .entries
        .into_iter()
        .filter_map(|entry| entry.subject)
        .collect();
    let mut live: Vec<String> = opened
        .bookmark_tips()?
        .into_keys()
        .map(|reference| reference.branch().to_string())
        .collect();
    live.extend(
        opened
            .conflicted_bookmarks()?
            .into_iter()
            .map(|(reference, _)| reference.branch().to_string()),
    );
    // The cut is already published and recorded: a state file that cannot be
    // read names no workspace rather than stopping the reap, and the reap
    // keeps every superseded name then.
    let claims = read_claims(bound);
    match &claims {
        Err(error) => {
            // A `StoreError` printed here, not through main's `error_line`:
            // it quotes the state file's path and serde's reading of it.
            println!(
                "  could not read claims, so no workspace is named for removal: {}",
                problem_human_text(&error.to_string())
            );
        }
        Ok(store) => {
            live.extend(
                store
                    .claims(Some(&fork.upstream))
                    .into_iter()
                    .map(|claim| claim.branch.clone()),
            );
            let orphans = release::workspaces_to_clean(&present, &recorded, &live);
            for line in orphan_workspace_lines(&orphans) {
                println!("{line}");
            }
        }
    }
    let exit = post_cut_exit.worst(reap_after_cut(fork, bound)?);
    if let Some(orphaned) = cut.stated_drop {
        report_stated_drop(fork, orphaned)?;
    }
    Ok(exit)
}

/// What a cut says about the jj workspaces whose branch is gone.
///
/// A workspace name is whatever its creator passed to `jj workspace add
/// --name`, and jj accepts a control character there, so each is escaped.
fn orphan_workspace_lines(orphans: &[String]) -> Vec<String> {
    if orphans.is_empty() {
        return vec!["  no workspace belongs to a branch that no longer exists".to_owned()];
    }
    let names: Vec<String> = orphans.iter().map(|name| inline_human_text(name)).collect();
    vec![
        format!(
            "  {} workspace(s) belong to branches that no longer exist: {}",
            names.len(),
            names.join(", ")
        ),
        "  no bookmark or live claim holds them; remove with `jj workspace forget \
         <name>` once you have checked them"
            .to_owned(),
    ]
}

/// Say what an `--allow-drop` cut dropped, judged after the reap: a commit the
/// cut does not carry is dropped only when it is no longer visible. The keep
/// rule usually keeps the superseded cut they rest on, and with it them.
fn report_stated_drop(fork: &Fork<'_>, orphaned: &OrphanedLineage) -> anyhow::Result<()> {
    let repo = knives::jj::Repo::open(&fork.checkout.path)?;
    let mut dropped = Vec::new();
    let mut still = Vec::new();
    for commit in &orphaned.commits {
        if repo.is_visible(commit)? {
            still.push(commit.short());
        } else {
            dropped.push(commit.short());
        }
    }
    let name = &fork.name;
    if dropped.is_empty() {
        println!(
            "{name}: --allow-drop: nothing dropped: the {} commit(s) the cut does not carry are still visible: {}",
            still.len(),
            still.join(", ")
        );
    } else {
        println!(
            "{name}: --allow-drop: dropped {} commit(s) the cut does not carry: {}",
            dropped.len(),
            dropped.join(", ")
        );
    }
    Ok(())
}

struct OrphanedLineage {
    previous: String,
    commits: Vec<knives::ids::CommitId>,
}

/// Refuse a cut that would strand `orphaned`'s commits unless `allow_drop`
/// names each of them; every commit is asked, so each value that names one
/// counts as used. Where it names each, the line that says so is stated
/// once every gate has asked ([`AllowDrop::settle`]).
fn report_orphaned_cut(
    repo: &RepoName,
    orphaned: &OrphanedLineage,
    allow_drop: &AllowDrop,
) -> Option<Exit> {
    // The previous release is a ref read from the repository.
    let previous = inline_human_text(&orphaned.previous);
    let mut every = true;
    for commit in &orphaned.commits {
        every &= allow_drop.commit(commit);
    }
    if every {
        allow_drop.state([format!(
            "{repo}: --allow-drop: cutting without {} commit(s) reachable only from {previous} \
             or its descendants; what is dropped is said once the cut is done",
            orphaned.commits.len()
        )]);
        return None;
    }
    println!(
        "{repo}: refusing to cut: {} commit(s) are reachable only from \
         {previous} or its descendants and would be dropped:",
        orphaned.commits.len()
    );
    for commit in &orphaned.commits {
        println!("    {}", commit.short());
    }
    println!("  re-run with --allow-drop to state this is intended");
    let names: Vec<String> = orphaned
        .commits
        .iter()
        .map(|commit| commit.short().to_owned())
        .collect();
    println!("{}", drop_flags_line(&names));
    Some(Exit::Incomplete)
}

/// The commits the cut would strand, reachable only from the previous
/// release or its descendants, or `None`.
fn check_orphan_commits_before_cut(
    opened: &knives::jj::Repo,
    fork: &Fork<'_>,
) -> anyhow::Result<Option<OrphanedLineage>> {
    let entry = fork.entry;
    let tips = opened.bookmark_tips()?;
    let Some(previous) = previous_release_for_cut(entry, &tips) else {
        return Ok(None);
    };
    let keep = release::cut_keepers(opened, entry, &tips, &previous.1)?;
    let orphans = release::orphaned_commits(release::OrphanedCommitInput {
        repo_path: &fork.checkout.path,
        previous: &previous.1,
        keep: &keep,
        tips: &tips,
        publish_remote: entry.publish_remote(),
    })?;
    if orphans.is_empty() {
        return Ok(None);
    }
    Ok(Some(OrphanedLineage {
        previous: previous.0.to_string(),
        commits: orphans,
    }))
}

/// Refuse a first cut while any branch it would compose carries a merge past
/// the trunk, naming each; `false` when every branch is linear.
fn refuse_stacked_first_cut(
    repo: &RepoName,
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    carried: &[(String, knives::ids::CommitId)],
) -> anyhow::Result<bool> {
    let stacked = stacked_branches(opened, entry, carried)?;
    for detail in &stacked {
        println!("{repo}: {detail}; rebase it off the trunk before cutting");
    }
    Ok(!stacked.is_empty())
}

/// What each branch of `carried` whose history carries a merge past the
/// trunk is told ([`knives::release_model::stacked_history`]), escaped for a
/// terminal: the detail names the branch and the release refs it stacks on.
/// A flat release composes none.
fn stacked_branches(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    carried: &[(String, knives::ids::CommitId)],
) -> anyhow::Result<Vec<String>> {
    let trunks = trunk_positions(opened, entry)?;
    let releases = knives::release_model::release_refs_by_commit(
        &opened.bookmark_tips()?,
        &entry.release_scheme(),
        entry.publish_remote(),
    );
    let context = StackedHistoryContext {
        repo: opened,
        trunks: &trunks,
        releases: &releases,
        carried: &[],
        written: &std::collections::BTreeSet::new(),
        replaced: None,
    };
    let mut stacked = Vec::new();
    for (branch, tip) in carried {
        if let Some(finding) = knives::release_model::stacked_history(context, branch, tip)? {
            stacked.push(inline_human_text(&finding.detail));
        }
    }
    Ok(stacked)
}

/// Reap superseded dated cuts now that a newer one exists, keeping each name
/// a release claim holds ([`reap`]).
///
/// Under `Fixed` the enumeration is empty by construction (no dated names), so
/// this is a no-op there. Opens the repository again deliberately: the caller's
/// handle predates the cut and reads stale tips, under which the superseded cut
/// is still the newest dated name and nothing is reaped at all.
fn reap_after_cut(fork: &Fork<'_>, bound: Bound<'_>) -> anyhow::Result<Exit> {
    let reaped = reap(fork, bound)?;
    print_reap(&fork.name, &reaped);
    Ok(reap_exit(&reaped.report))
}

/// This machine's state file, read for its claims as the cut and the reap
/// read it: with the registry's former names, and no fork's ledger.
fn read_claims(bound: Bound<'_>) -> Result<knives::store::Store, knives::store::StoreError> {
    knives::store::Store::open(
        knives::store::default_state_path(),
        &bound.registry.former_names(),
        &[],
    )
}

/// The release claims of `fork` in the state file `read` holds: each claim
/// of a name that is a release under the fork's scheme, with its owner, and
/// each name of `busy`, a `start` or `finish` of which holds its lock now;
/// or why the file could not be read ([`release::reap_superseded_held`]).
fn release_claims(
    fork: &Fork<'_>,
    read: &Result<knives::store::Store, knives::store::StoreError>,
    busy: Vec<String>,
) -> release::ReleaseClaims {
    let store = match read {
        Ok(store) => store,
        Err(error) => return release::ReleaseClaims::Unreadable(error.to_string()),
    };
    let scheme = fork.entry.release_scheme();
    release::ReleaseClaims::Read(
        store
            .claims(Some(&fork.upstream))
            .into_iter()
            .filter(|claim| {
                knives::ids::is_release_name(&BranchName::new(claim.branch.as_str()), &scheme)
            })
            .map(|claim| release::HeldRelease {
                name: claim.branch.clone(),
                holder: Some(claim.owner.clone()),
            })
            .chain(
                busy.into_iter()
                    .map(|name| release::HeldRelease { name, holder: None }),
            )
            .collect(),
    )
}

/// The lock of each release name `opened` holds a ref of, tried under the
/// fork's lock the reap holds, which waits for none of them: the locks it
/// took, held until the reap is done, and the names a `start` or `finish`
/// holds now, which the reap keeps.
///
/// A release claim is taken and released by a `start` or `finish` of its
/// name under that name's lock, not the fork's ([`BranchLock`]), so this is
/// what keeps a claim from being taken between the reap's read of the
/// claims and its forget of the name.
fn lock_release_names(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
) -> anyhow::Result<(Vec<BranchLock>, Vec<String>)> {
    let scheme = fork.entry.dated_scheme();
    let names: std::collections::BTreeSet<String> = opened
        .bookmark_tips()?
        .into_keys()
        .map(|reference| reference.branch().as_str().to_owned())
        .filter(|name| knives::ids::is_release_name(&BranchName::new(name.as_str()), &scheme))
        .collect();
    let (mut held, mut busy) = (Vec::new(), Vec::new());
    for name in names {
        match BranchLock::try_acquire(&fork.upstream, &BranchName::new(name.as_str()))? {
            Some(lock) => held.push(lock),
            None => busy.push(name),
        }
    }
    Ok((held, busy))
}

/// Abandon the earlier writes of each name the reap just forgot — commits the
/// ledger records knives writing under it, and their visible rewrites — that
/// are visible heads nothing rests on: a replaced commit once kept for a
/// working copy that has since moved on would otherwise stay an anonymous head
/// for good. Returns each abandoned commit with its name, each head those
/// abandons exposed and hid with its name, and a note for each name whose
/// cleanup failed: the reap itself is already committed, so its report must
/// still reach the operator.
fn abandon_reaped_writes(
    fork: &Fork<'_>,
    report: &knives::commands::release::ReapReport,
    ledger: &[knives::ledger::Entry],
) -> ReapedWrites {
    let names: Vec<&str> = report
        .reaped
        .iter()
        .chain(report.forgotten_only.iter().map(|(name, _)| name))
        .map(String::as_str)
        .collect();
    let mut done = ReapedWrites::default();
    if names.is_empty() {
        return done;
    }
    let reopened = match knives::jj::Repo::open(&fork.checkout.path) {
        Ok(reopened) => reopened,
        Err(error) => {
            done.notes
                .push(format!("earlier writes not cleaned up: {error}"));
            return done;
        }
    };
    // The live cut's conflicts stop the hiding: its superseded cuts are then
    // the record of their last resolution.
    let live = reopened.bookmark_tips().ok().and_then(|tips| {
        release::live_dated_release(
            &tips,
            &fork.entry.dated_scheme(),
            fork.entry.publish_remote(),
        )
    });
    for name in names {
        let abandoned = release::earlier_writes(&reopened, ledger, name).and_then(|earlier| {
            knives::jj::abandon_unheld_writes(
                &fork.checkout.path,
                &earlier,
                live.as_ref().map(|(_, commit)| commit),
                &format!("knives: {name}: abandon earlier writes nothing rests on"),
            )
        });
        match abandoned {
            Ok(abandoned) => {
                done.abandoned.extend(
                    abandoned
                        .abandoned
                        .into_iter()
                        .map(|commit| (name.to_owned(), commit)),
                );
                done.hidden.extend(
                    abandoned
                        .hidden
                        .into_iter()
                        .map(|head| (name.to_owned(), head)),
                );
            }
            Err(error) => done
                .notes
                .push(format!("{name}: earlier writes not cleaned up: {error}")),
        }
    }
    done
}

/// What [`abandon_reaped_writes`] did, each commit with the name it was a
/// write of, and the notes of each name whose cleanup failed.
#[derive(Default)]
struct ReapedWrites {
    abandoned: Vec<(String, knives::ids::CommitId)>,
    hidden: Vec<(String, knives::jj::HiddenHead)>,
    notes: Vec<String>,
}

/// Reap superseded dated cuts on demand, in the chosen output, keeping each
/// name a release claim in this machine's state file holds.
pub(crate) fn run_reap(
    fork: &Fork<'_>,
    output: knives::cli::Output,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let reaped = reap(fork, bound)?;
    if let Some(payload) = knives::cli::machine_payload(output, &ReapPayload::of(fork, &reaped))? {
        println!("{payload}");
    } else {
        print_reap(&fork.name, &reaped);
    }
    Ok(reap_exit(&reaped.report))
}

/// What one reap did: the report, the earlier writes it abandoned after, and
/// every head its abandons exposed and hid, each with its name.
struct Reaped {
    report: knives::commands::release::ReapReport,
    abandoned_earlier: Vec<(String, knives::ids::CommitId)>,
    hidden: Vec<(String, knives::jj::HiddenHead)>,
}

/// The reap both callers run, on the repository as it is now. The ledger's
/// earlier writes of each name count for the keep rule and are cleaned up
/// after it, and a name a release claim holds is kept: the claims are read
/// once the lock of each release name is taken, or kept as busy
/// ([`lock_release_names`]).
fn reap(fork: &Fork<'_>, bound: Bound<'_>) -> anyhow::Result<Reaped> {
    let path = &fork.checkout.path;
    let (scheme, publish_remote) = (fork.entry.dated_scheme(), fork.entry.publish_remote());
    let opened = knives::jj::Repo::open(path)?;
    let (_names, busy) = lock_release_names(fork, &opened)?;
    let claims = release_claims(fork, &read_claims(bound), busy);
    let ledger = Ledger::for_fork(fork).read()?.entries;
    let mut report =
        release::reap_superseded_held(&opened, &scheme, publish_remote, &ledger, &claims)?;
    let cleaned = abandon_reaped_writes(fork, &report, &ledger);
    report.notes.extend(cleaned.notes);
    // The reap's own abandons hid what they exposed in one operation over
    // every reaped name, so those heads are named with all of them.
    let reaped_names = report.reaped.join(", ");
    let mut hidden: Vec<(String, knives::jj::HiddenHead)> = std::mem::take(&mut report.hidden)
        .into_iter()
        .map(|head| (reaped_names.clone(), head))
        .collect();
    hidden.extend(cleaned.hidden);
    Ok(Reaped {
        report,
        abandoned_earlier: cleaned.abandoned,
        hidden,
    })
}

/// A reap in its machine encoding: the same facts as the prose, by field.
#[derive(serde::Serialize)]
struct ReapPayload {
    repo: String,
    /// Names whose refs were forgotten and commits abandoned.
    reaped: Vec<String>,
    /// Names whose refs were forgotten while a pin kept the commit.
    forgotten_only: Vec<NamedReason>,
    /// Names left untouched, with why.
    kept: Vec<NamedReason>,
    /// Abandons that failed for a reason other than a pin, and failed cleanups
    /// of a reaped name's earlier writes.
    notes: Vec<String>,
    /// Earlier writes of a reaped name abandoned because nothing rests on them.
    abandoned_earlier: Vec<AbandonedWrite>,
    /// Heads the reap's abandons exposed and hid in the same operation: an
    /// earlier write of a reaped name, or a commit a visible rewrite
    /// supersedes.
    hidden: Vec<HiddenWrite>,
}

#[derive(serde::Serialize)]
struct NamedReason {
    name: String,
    reason: String,
}

#[derive(serde::Serialize)]
struct AbandonedWrite {
    name: String,
    commit: String,
}

#[derive(serde::Serialize)]
struct HiddenWrite {
    name: String,
    commit: String,
    /// The visible commit that rewrote it; absent for an earlier write.
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<String>,
}

impl ReapPayload {
    fn of(fork: &Fork<'_>, reaped: &Reaped) -> Self {
        let named = |pairs: &[(String, String)]| -> Vec<NamedReason> {
            pairs
                .iter()
                .map(|(name, reason)| NamedReason {
                    name: name.clone(),
                    reason: reason.clone(),
                })
                .collect()
        };
        Self {
            repo: fork.name.to_string(),
            reaped: reaped.report.reaped.clone(),
            forgotten_only: named(&reaped.report.forgotten_only),
            kept: named(&reaped.report.kept),
            notes: reaped.report.notes.clone(),
            abandoned_earlier: reaped
                .abandoned_earlier
                .iter()
                .map(|(name, commit)| AbandonedWrite {
                    name: name.clone(),
                    commit: commit.as_str().to_owned(),
                })
                .collect(),
            hidden: reaped
                .hidden
                .iter()
                .map(|(name, head)| HiddenWrite {
                    name: name.clone(),
                    commit: head.commit.as_str().to_owned(),
                    superseded_by: head.superseded_by.as_ref().map(|by| by.as_str().to_owned()),
                })
                .collect(),
        }
    }
}

/// Return a finding when reaping leaves a cut behind because something rests
/// on it — someone's stacked work, or a workspace's working copy — or a live
/// release claim holds its name, or the claims could not be read, or the
/// reap could not finish. A commit kept because a tag or another bookmark
/// still pins it is none of these: every ref knives owns is gone and the
/// commit stays on purpose, so there is nothing to act on, and a cut whose
/// reap met one must not exit non-zero for as long as the pin exists.
/// Neither is a cut kept only for what its own name published on it:
/// consumers were given that, and the newer cut redoes it.
fn reap_exit(report: &knives::commands::release::ReapReport) -> Exit {
    let acted_on = report
        .kept
        .iter()
        .any(|(name, _)| !report.kept_published.contains(name));
    if !acted_on && report.notes.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

fn print_reap(repo: &RepoName, reaped: &Reaped) {
    for line in reap_lines(repo, &reaped.report) {
        println!("{line}");
    }
    for (name, commit) in &reaped.abandoned_earlier {
        super::release_edit::report_abandoned_earlier(repo, name, std::slice::from_ref(commit));
    }
    for (name, head) in &reaped.hidden {
        report_hidden(repo, name, std::slice::from_ref(head));
    }
}

/// The lines a reap reports. Each name is a release ref read from the
/// repository; a reason names the bookmarks, tags and jj workspaces that keep
/// a cut (`what_rests_on`), and a note quotes jj's error. The machine payload
/// keeps all of it raw.
fn reap_lines(repo: &RepoName, report: &knives::commands::release::ReapReport) -> Vec<String> {
    let mut lines = Vec::new();
    if report.reaped.is_empty() && report.forgotten_only.is_empty() && report.kept.is_empty() {
        lines.push(format!("{repo}: nothing to reap"));
    }
    for name in &report.reaped {
        lines.push(format!(
            "{repo}: reaped {} (refs forgotten everywhere, commit abandoned; remote untouched)",
            inline_human_text(name)
        ));
    }
    for (name, why) in &report.forgotten_only {
        lines.push(format!(
            "{repo}: reaped {} (refs forgotten everywhere; commit kept, {})",
            inline_human_text(name),
            inline_human_text(why)
        ));
    }
    for (name, reason) in &report.kept {
        lines.push(format!(
            "{repo}: kept {}: {}",
            inline_human_text(name),
            inline_human_text(reason)
        ));
    }
    for note in &report.notes {
        lines.push(format!("{repo}: ! {}", problem_human_text(note)));
    }
    lines
}

fn print_previous_release_position(opened: &knives::jj::Repo, entry: &knives::config::RepoEntry) {
    if let Some((reference, commit)) = release::previous_position(opened, entry) {
        println!(
            "  previous release position: {reference} at {}",
            commit.short()
        );
    } else if matches!(entry.release_scheme(), ReleaseScheme::Fixed(_)) {
        println!("  no previous release position: this is the first cut of the fixed branch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knives::commands::release::{
        AbsentAdopt, CompositionCheck, FixReference, HeldFile, LackedFix, ReapReport,
        RecordedMember,
    };
    use knives::ids::CommitId;

    /// ESC opens a C0 sequence, U+009B is the C1 CSI; neither may reach a
    /// terminal.
    const HOSTILE: &str = "x\u{1b}]0;pwned\u{7}\u{9b}2J";
    const ESCAPED: &str = "x\u{fffd}]0;pwned\u{fffd}\u{fffd}2J";

    /// The closing line of a refusal's `lines`: the line before the last,
    /// which gives the `--allow-drop` of each drop ([`drop_flags_line`]).
    fn closing(lines: &[String]) -> Option<&String> {
        let (flags, rest) = lines.split_last()?;
        assert!(flags.starts_with("  name each drop: "), "{lines:?}");
        rest.last()
    }

    fn assert_escaped(lines: &[String]) {
        for line in lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
    }

    /// Remotes read already: origin, which has no branch or tag at any
    /// commit.
    fn no_remote() -> Remotes<'static> {
        Remotes::read_as(Advertised {
            refs: vec![("origin", BTreeMap::new())],
            unread: Vec::new(),
        })
    }

    /// Remotes read already, where origin has `reference` at `commit`.
    fn origin_has(reference: &str, commit: &str) -> Remotes<'static> {
        Remotes::read_as(Advertised {
            refs: vec![(
                "origin",
                BTreeMap::from([(reference.to_owned(), CommitId::new(commit))]),
            )],
            unread: Vec::new(),
        })
    }

    #[test]
    fn a_recorded_cut_from_the_ledger_reaches_the_terminal_escaped() {
        // Each record's name is a ledger entry's subject and its commit the
        // entry's first evidence string: a pulled peer ledger writes both.
        let cut = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: format!("release/{HOSTILE}"),
            commit: CommitId::new(HOSTILE),
            members: vec![CommitId::new(HOSTILE), CommitId::new("aaaa")],
        };
        let write = RecordedCut {
            kind: RecordedWriteKind::Edit,
            name: format!("release/{HOSTILE}.2"),
            commit: CommitId::new(HOSTILE),
            members: vec![CommitId::new(HOSTILE), CommitId::new("abab")],
        };
        let repo = RepoName::new("demo");
        let check = CompositionCheck {
            dropped: vec![
                RecordedMember {
                    commit: CommitId::new(HOSTILE),
                    name: format!("{HOSTILE} (not known to this repository)"),
                },
                RecordedMember {
                    commit: CommitId::new("abab"),
                    name: format!("feat/{HOSTILE}@abab"),
                },
            ],
            inconclusive: vec![RecordedMember {
                commit: CommitId::new("aaaa"),
                name: format!("feat/{HOSTILE}@aaaa"),
            }],
            twins: vec![(format!("feat/{HOSTILE}@bbbb"), CommitId::new("cccc"))],
            absent_write: true,
            absent_adopts: vec![AbsentAdopt {
                name: format!("release/{HOSTILE}"),
                commit: CommitId::new(HOSTILE),
            }],
            ..CompositionCheck::default()
        };

        let published = recorded_published_line(
            &repo,
            ("release/2026-11-13", "origin"),
            &CommitId::new("dddd"),
            Some(&cut),
        );
        let mut lines = vec![published.clone()];
        let at = CommitId::new("eeee");
        let gates = [
            HeldRecords {
                first: HeldRecord::PreviousCut(&cut),
                other: Some(HeldRecord::NewestWrite(&write)),
                record: None,
                past: None,
            },
            HeldRecords {
                first: HeldRecord::NewestWrite(&write),
                other: Some(HeldRecord::PreviousCut(&cut)),
                record: Some(("release/2026-11-14", &at)),
                past: None,
            },
        ];
        let none = no_remote();
        for held in gates {
            for on_drop in [
                DropDisposition::Allowed,
                DropDisposition::RefuseDiscardingCandidate,
            ] {
                lines.extend(uncarried_cut_lines(&repo, held, &check, (on_drop, &none)));
            }
        }

        assert_escaped(&lines);
        assert!(
            published.ends_with(&format!(
                "previous recorded cut release/{ESCAPED} was {}",
                ids_prefix(ESCAPED)
            )),
            "{published:?}"
        );
        let text = lines.join("\n");
        let short = ids_prefix(ESCAPED);
        for expected in [
            format!("the previous cut release/{ESCAPED} recorded"),
            format!("which release/{ESCAPED}.2's newest recorded write (an edit) holds"),
            format!("of release/{ESCAPED}.2, the newest release that the ledger records"),
            format!("    {ESCAPED} (not known to this repository)"),
            format!("    feat/{ESCAPED}@abab"),
            format!("feat/{ESCAPED}@aaaa: recorded by the previous cut release/{ESCAPED}"),
            format!("feat/{ESCAPED}@bbbb is carried by cccc"),
            format!(
                "cannot be held against release/{ESCAPED}.2's newest recorded write (an edit) \
                 {short}: this repository does not hold that commit"
            ),
            format!("`jj bookmark set 'release/{ESCAPED}.2' -r '"),
            format!("`knives release adopt 'release/{ESCAPED}.2'` records the commit"),
            format!(
                "the fix by hand of release/{ESCAPED}'s adopt {short} holds nothing here: this \
                 repository does not hold {short}"
            ),
            "name each drop: --allow-drop ".to_owned(),
        ] {
            assert!(text.contains(&expected), "{expected:?} not in {text:?}");
        }
    }

    #[test]
    fn a_lacked_fix_from_the_ledger_reaches_the_terminal_escaped() {
        // Given: a record that lacks a file of the fix by hand of an adopt,
        // whose release and commit, and the newest write's, are a ledger
        // entry's subject and first evidence string, which a pulled peer
        // ledger writes, held against that newest write.
        let write = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: format!("release/{HOSTILE}.2"),
            commit: CommitId::new(HOSTILE),
            members: Vec::new(),
        };
        let check = CompositionCheck {
            dropped_fixes: vec![LackedFix {
                name: format!("release/{HOSTILE}"),
                commit: CommitId::new(HOSTILE),
                files: vec![held_file(
                    &format!("fix{HOSTILE}.txt"),
                    FixReference::Conflicted,
                )],
            }],
            ..CompositionCheck::default()
        };
        let at = CommitId::new("eeee");
        let held = HeldRecords {
            first: HeldRecord::NewestWrite(&write),
            other: None,
            record: Some(("release/2026-11-14", &at)),
            past: None,
        };

        // When: the gate says what it found, refused and allowed.
        let repo = RepoName::new("demo");
        let mut lines = uncarried_cut_lines(
            &repo,
            held,
            &check,
            (DropDisposition::RefuseRecordingNothing, &no_remote()),
        );
        lines.extend(uncarried_cut_lines(
            &repo,
            held,
            &check,
            (DropDisposition::Allowed, &no_remote()),
        ));

        // Then: every line is escaped, and names the adopt, the file it
        // lacks and the adopt that keeps the new content.
        assert_escaped(&lines);
        let text = lines.join("\n");
        let short = ids_prefix(ESCAPED);
        for expected in [
            format!(
                "it does not carry the fix by hand of release/{ESCAPED}'s adopt {short}, in 1 \
                 file(s): fix{ESCAPED}.txt"
            ),
            format!(
                "to keep the new content of those files, edit release/{ESCAPED}.2 by hand to hold \
                 it and run `knives release adopt 'release/{ESCAPED}.2'`"
            ),
            "--allow-drop: recording release/2026-11-14 at eeee without the fix by hand".to_owned(),
        ] {
            assert!(text.contains(&expected), "{expected:?} not in {text:?}");
        }
    }

    #[test]
    fn a_lacked_file_with_no_reference_here_names_the_remote_ref_to_fetch_escaped() {
        // Given: a record that lacks a file of the fix by hand of an adopt
        // that has no reference here, as the walk for it stopped at a write
        // that this repository does not hold, whose release and commit are a
        // ledger entry's subject and first evidence string, which a pulled
        // peer ledger writes.
        let write = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-11-13".to_owned(),
            commit: CommitId::new("bbbb"),
            members: Vec::new(),
        };
        let unread = FixReference::Unread {
            name: format!("release/{HOSTILE}.2"),
            commit: CommitId::new(HOSTILE),
        };
        let check = CompositionCheck {
            dropped_fixes: vec![LackedFix {
                name: "release/2026-11-12".to_owned(),
                commit: CommitId::new("aaaa"),
                files: vec![
                    held_file("fix.txt", unread.clone()),
                    held_file("other.txt", unread),
                ],
            }],
            ..CompositionCheck::default()
        };
        let at = CommitId::new("eeee");
        let held = HeldRecords {
            first: HeldRecord::NewestWrite(&write),
            other: None,
            record: Some(("release/2026-11-14", &at)),
            past: None,
        };
        let refuse = DropDisposition::RefuseRecordingNothing;

        // When: the gate refuses the record where origin has a branch at
        // that write, a ref that a remote names, and where neither origin
        // nor the publish remote has one.
        let advertised = origin_has(&format!("refs/heads/release/{HOSTILE}.2"), HOSTILE);
        let fetched =
            uncarried_cut_lines(&RepoName::new("demo"), held, &check, (refuse, &advertised));
        let unfetched =
            uncarried_cut_lines(&RepoName::new("demo"), held, &check, (refuse, &no_remote()));

        // Then: where origin has it, the remedy names that remote and ref
        // once, escaped, with the commit and its fetch, then the commit that
        // carries the fix and the adopt that keeps the new content; where no
        // remote has it, no fetch and no commit can meet those files, so the
        // adopt of the new content and `--allow-drop` are the way on.
        assert_escaped(&fetched);
        assert_escaped(&unfetched);
        let keep = "to keep the new content of those files, edit release/2026-11-13 by hand to \
                    hold it and run `knives release adopt release/2026-11-13`";
        assert_eq!(
            closing(&fetched).map(String::as_str),
            Some(
                format!(
                    "  nothing was recorded; fetch origin's refs/heads/release/{ESCAPED}.2, which \
                     holds {}, then record a commit that carries that fix; {keep}, or re-run with \
                     --allow-drop to state the drop is intended",
                    ids_prefix(ESCAPED)
                )
                .as_str()
            )
        );
        assert_eq!(
            closing(&unfetched).map(String::as_str),
            Some(
                format!(
                    "  nothing was recorded; {keep}, or re-run with --allow-drop to state the drop \
                     is intended"
                )
                .as_str()
            )
        );
        assert_eq!(
            fetched.last().map(String::as_str),
            Some("  name each drop: --allow-drop fix.txt --allow-drop other.txt")
        );
    }

    #[test]
    fn a_remote_tag_names_the_commit_it_peels_to_from_a_short_commit() {
        // Given: origin with a tag object `kept` whose peeled commit the
        // ledger gives short, and an empty commit.
        let advertised = Advertised {
            refs: vec![(
                "origin",
                BTreeMap::from([
                    ("refs/tags/kept".to_owned(), CommitId::new("ffff")),
                    (
                        "refs/tags/kept^{}".to_owned(),
                        CommitId::new("0123456789abcdef0123456789abcdef01234567"),
                    ),
                ]),
            )],
            unread: Vec::new(),
        };

        // Then: the short commit is found at the tag, peeled; an empty one
        // names nothing.
        assert_eq!(
            advertised.at(&CommitId::new("0123456789ab")),
            Some(("origin", "refs/tags/kept"))
        );
        assert_eq!(advertised.at(&CommitId::new("")), None);
    }

    /// The fix by hand of `release/2026-11-12`'s adopt `aaaa` at `fix.txt`,
    /// whose reference is the newest write, `bbbb`.
    fn held_fix() -> LackedFix {
        LackedFix {
            name: "release/2026-11-12".to_owned(),
            commit: CommitId::new("aaaa"),
            files: vec![held_file(
                "fix.txt",
                FixReference::Write(CommitId::new("bbbb")),
            )],
        }
    }

    #[test]
    fn a_refusal_over_a_member_and_a_fix_moves_before_it_includes_and_names_each_drop() {
        // Given: the newest write, `release/2026-11-13`'s cut `bbbb`, which
        // records the member `feat/alpha@abab`; the previous cut,
        // `release/2026-11-10`'s `cccc`, which records `feat/alpha@cdcd`;
        // and the fix by hand of an adopt at a file whose reference is the
        // newest write.
        let write = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-11-13".to_owned(),
            commit: CommitId::new("bbbb"),
            members: vec![CommitId::new("abab")],
        };
        let previous = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-11-10".to_owned(),
            commit: CommitId::new("cccc"),
            members: vec![CommitId::new("cdcd")],
        };
        let at = CommitId::new("eeee");
        let record = HeldRecords {
            first: HeldRecord::NewestWrite(&write),
            other: None,
            record: Some(("release/2026-11-14", &at)),
            past: None,
        };
        let cut = HeldRecords {
            record: None,
            ..record
        };
        let after_cut = HeldRecords {
            first: HeldRecord::PreviousCut(&previous),
            other: Some(HeldRecord::NewestWrite(&write)),
            ..cut
        };
        let lacking = |member: &str| CompositionCheck {
            dropped: vec![RecordedMember {
                commit: CommitId::new(member),
                name: format!("feat/alpha@{member}"),
            }],
            dropped_fixes: vec![held_fix()],
            ..CompositionCheck::default()
        };
        let repo = RepoName::new("demo");
        let none = no_remote();
        let refuse = |held: HeldRecords<'_>, check: &CompositionCheck| {
            let on_drop = if held.record.is_some() {
                DropDisposition::RefuseRecordingNothing
            } else {
                DropDisposition::RefuseDiscardingCandidate
            };
            let lines = uncarried_cut_lines(&repo, held, check, (on_drop, &none));
            assert_escaped(&lines);
            (closing(&lines).cloned(), lines.last().cloned())
        };

        // When: the gate refuses a record and a cut that lack the member of
        // the newest write and the fix, and a cut that lacks the member of
        // the previous cut and the fix.
        let got = [
            refuse(record, &lacking("abab")),
            refuse(cut, &lacking("abab")),
            refuse(after_cut, &lacking("cdcd")),
        ];

        // Then: `include` follows the move, and the line after the remedy
        // gives the `--allow-drop` of the member and of the fix's file.
        let allow = "or re-run with --allow-drop to state the drop is intended";
        let moved = "move release/2026-11-13 to bbbb, its newest recorded write: `jj bookmark set \
                     release/2026-11-13 -r bbbb --allow-backwards`";
        let include = "`knives release include <branch>` restores a member the previous cut \
                       recorded";
        let keep = "to keep the new content of those files, edit release/2026-11-13 by hand to \
                    hold it and run `knives release adopt release/2026-11-13`";
        let (recorded, discarded) = ("  nothing was recorded", "  the candidate was discarded");
        let flags = |member: &str| {
            Some(format!(
                "  name each drop: --allow-drop {member} --allow-drop fix.txt"
            ))
        };
        let wanted = [
            (
                Some(format!(
                    "{recorded}; record a commit that carries each of them and each fix; {keep}, \
                     {allow}"
                )),
                flags("abab"),
            ),
            (
                Some(format!("{discarded}; fetch, then {moved}; {keep}, {allow}")),
                flags("abab"),
            ),
            (
                Some(format!(
                    "{discarded}; fetch, then {moved}; {include}; {keep}, {allow}"
                )),
                flags("cdcd"),
            ),
        ];
        assert_eq!(got, wanted);
    }

    #[test]
    fn a_refused_cut_moves_to_the_newest_write_before_it_includes_a_member_of_the_previous_cut() {
        // Given: the previous cut of `release/2026-08-04`, `dddd`, which
        // records `feat/gamma`; that release's newest recorded write, the
        // edit `eeee`, which holds `feat/beta` too; and a cut that lacks
        // both members. The release here is older than the edit, as on a
        // machine that has not fetched it.
        let (gamma, beta) = (CommitId::new("cccc"), CommitId::new("bbbb"));
        let cut = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("dddd"),
            members: vec![gamma.clone()],
        };
        let write = RecordedCut {
            kind: RecordedWriteKind::Edit,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("eeee"),
            members: vec![gamma.clone(), beta.clone()],
        };
        let check = CompositionCheck {
            dropped: vec![
                RecordedMember {
                    commit: gamma,
                    name: "feat/gamma@cccc".to_owned(),
                },
                RecordedMember {
                    commit: beta,
                    name: "feat/beta@bbbb".to_owned(),
                },
            ],
            ..CompositionCheck::default()
        };
        let held = HeldRecords {
            first: HeldRecord::PreviousCut(&cut),
            other: Some(HeldRecord::NewestWrite(&write)),
            record: None,
            past: None,
        };

        // When: the gate refuses the cut.
        let lines = uncarried_cut_lines(
            &RepoName::new("demo"),
            held,
            &check,
            (DropDisposition::RefuseDiscardingCandidate, &no_remote()),
        );

        // Then: its remedy moves the release to the newest write first, and
        // `include` restores the previous cut's member after that. An
        // `include` on the older release here is not refused: it matches an
        // older recorded write, so it would write a second write beside the
        // edit, divergent from it, which the move would then leave behind.
        assert_eq!(
            closing(&lines).map(String::as_str),
            Some(
                "  the candidate was discarded; fetch, then move release/2026-08-04 to eeee, its \
                 newest recorded write: `jj bookmark set release/2026-08-04 -r eeee \
                 --allow-backwards`; `knives release include <branch>` restores a member the \
                 previous cut recorded, or re-run with --allow-drop to state the drop is intended"
            )
        );
    }

    #[test]
    fn a_refused_cut_names_the_adopt_that_takes_the_moves_place_before_it_includes_a_member() {
        // Given: the previous cut of `release/2026-08-04`, `dddd`, which
        // records `feat/gamma`; that release's newest recorded write, the
        // edit `eeee`, which this repository does not hold; and a cut that
        // lacks `feat/gamma`.
        let gamma = CommitId::new("cccc");
        let cut = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("dddd"),
            members: vec![gamma.clone()],
        };
        let write = RecordedCut {
            kind: RecordedWriteKind::Edit,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("eeee"),
            members: vec![gamma.clone()],
        };
        let check = CompositionCheck {
            dropped: vec![RecordedMember {
                commit: gamma,
                name: "feat/gamma@cccc".to_owned(),
            }],
            absent_write: true,
            ..CompositionCheck::default()
        };
        let held = HeldRecords {
            first: HeldRecord::PreviousCut(&cut),
            other: Some(HeldRecord::NewestWrite(&write)),
            record: None,
            past: None,
        };

        // When: the gate refuses the cut.
        let lines = uncarried_cut_lines(
            &RepoName::new("demo"),
            held,
            &check,
            (DropDisposition::RefuseDiscardingCandidate, &no_remote()),
        );

        // Then: its remedy moves the release to the newest write after a
        // fetch, names the `adopt` that takes the move's place where the
        // fetch does not bring that write, and only then the `include` that
        // restores the previous cut's member. Where the move fails, an
        // `include` on the older release here would write a second write,
        // divergent from the one this repository does not hold.
        assert_eq!(
            closing(&lines).map(String::as_str),
            Some(
                "  the candidate was discarded; fetch, then move release/2026-08-04 to eeee, its \
                 newest recorded write: `jj bookmark set release/2026-08-04 -r eeee \
                 --allow-backwards`; if the fetch does not bring eeee, `knives release adopt \
                 release/2026-08-04` records the commit this repository holds as \
                 release/2026-08-04's newest write; `knives release include <branch>` restores a \
                 member the previous cut recorded, or re-run with --allow-drop to state the drop \
                 is intended"
            )
        );
    }

    #[test]
    fn a_refusal_never_moves_a_release_behind_what_its_publish_remote_holds() {
        // Given: the fixed branch `integration`, whose newest write, the edit
        // `bbbb`, is the reference of a fix by hand that the cut lacks; and
        // the publish remote's commit of the branch, `9999`, which holds
        // commits that write lacks, as oh-my-pi's Cut9 holds Cut7's.
        let write = RecordedCut {
            kind: RecordedWriteKind::Edit,
            name: "integration".to_owned(),
            commit: CommitId::new("bbbb"),
            members: Vec::new(),
        };
        let published = CommitId::new("9999");
        let refuse = |past: Option<PublishedPast<'_>>| {
            let held = HeldRecords {
                first: HeldRecord::NewestWrite(&write),
                other: None,
                record: None,
                past,
            };
            let check = CompositionCheck {
                dropped_fixes: vec![held_fix()],
                absent_write: past.is_some_and(|past| !past.held),
                ..CompositionCheck::default()
            };
            let lines = uncarried_cut_lines(
                &RepoName::new("demo"),
                held,
                &check,
                (DropDisposition::RefuseRecordingNothing, &no_remote()),
            );
            assert_escaped(&lines);
            closing(&lines).cloned().unwrap_or_default()
        };
        let past = |held| PublishedPast {
            remote: "origin",
            commit: &published,
            held,
        };

        // When: the gate refuses the cut where this repository holds the
        // write, where it does not, and (control) where the publish remote
        // holds nothing past the write.
        let (holds, lacks, control) = (
            refuse(Some(past(true))),
            refuse(Some(past(false))),
            refuse(None),
        );

        // Then: neither refusal past the published commit names the move
        // back to the write or an edit of the branch by hand, which would
        // rewrite that commit. Where the write is held, it names the record
        // of the published commit as it is, each drop named, then the
        // fix-forward: a merge of the published commit and a branch that
        // carries the fix, pushed, then the cut again; it gives no second
        // `--allow-drop` clause. Where it is not, the fetch and the cut
        // again. The control names both.
        let allow = "or re-run with --allow-drop to state the drop is intended";
        assert_eq!(
            holds,
            "  nothing was recorded; integration@origin (9999) holds commits that bbbb, its newest \
             recorded write, lacks, and knives never moves integration behind its publish \
             remote: re-run with --allow-drop naming each drop below, which records 9999 as it \
             is and names each drop; then, to carry what this cut lacks, make integration a \
             merge of 9999 and a branch that carries it (`jj new 9999 <branch> -m 'integration, \
             fixed forward on 9999'`, `jj bookmark set integration -r @`, then `jj new \
             'trunk()'`), push integration, then cut again, which records that merge with 9999 as \
             its carrier"
        );
        assert_eq!(
            lacks,
            format!(
                "  nothing was recorded; fetch, then cut again: this repository does not hold \
                 bbbb, integration's newest recorded write, so it cannot tell whether that write \
                 holds integration@origin (9999), and knives never moves integration behind its \
                 publish remote; if the fetch does not bring bbbb, `knives release adopt \
                 integration` records the commit this repository holds as integration's newest \
                 write, {allow}"
            )
        );
        for moved in ["--allow-backwards", "edit integration by hand"] {
            assert!(control.contains(moved), "{control}");
            assert!(
                !holds.contains(moved) && !lacks.contains(moved),
                "{holds}\n{lacks}"
            );
        }
    }

    #[test]
    fn a_record_lacking_a_fix_at_files_with_no_reference_is_not_asked_for_a_commit_that_carries_it()
    {
        // Given: the fix by hand of an adopt that a record lacks at a file
        // that no recorded write holds with no conflict, so the file has no
        // reference at all; the same fix lacked also at a file whose
        // reference is the newest write; the same fix at a file with no
        // reference that the adopt's change changes; and the first beside
        // the fix of another adopt that a commit can carry.
        let fix = |files: Vec<HeldFile>| LackedFix {
            files,
            ..held_fix()
        };
        let unreferenced = || held_file("fix.txt", FixReference::Conflicted);
        let referenced = held_file("other.txt", FixReference::Write(CommitId::new("bbbb")));
        let replayed = HeldFile {
            replayed: true,
            ..unreferenced()
        };
        let another = LackedFix {
            name: "release/2026-11-11".to_owned(),
            commit: CommitId::new("cccc"),
            ..held_fix()
        };

        // When: the gate refuses each record.
        let none = no_remote();
        let alone = fix_refusal(vec![fix(vec![unreferenced()])], &none);
        let mixed = fix_refusal(vec![fix(vec![unreferenced(), referenced])], &none);
        let changed = fix_refusal(vec![fix(vec![replayed])], &none);
        let beside = fix_refusal(vec![fix(vec![unreferenced()]), another], &none);

        // Then: the record that lacks the fix at the file with no reference
        // alone is not asked for a commit that carries it; it keeps the adopt
        // of the new content, which gives that file a reference, and
        // `--allow-drop`. Where a commit can carry the fix at a file, by its
        // reference or by a replay of the adopt's change, the commit step
        // stays, and only the fixes that a commit can carry are counted.
        let keep = "to keep the new content of those files, edit release/2026-11-13 by hand to \
                    hold it and run `knives release adopt release/2026-11-13`";
        let allow = "or re-run with --allow-drop to state the drop is intended";
        assert_eq!(
            alone,
            [
                "demo: refusing to record release/2026-11-14 at eeee: it does not carry the fix by \
                 hand of release/2026-11-12's adopt aaaa, in 1 file(s): fix.txt"
                    .to_owned(),
                format!("  nothing was recorded; {keep}, {allow}"),
                "  name each drop: --allow-drop fix.txt".to_owned(),
            ]
        );
        let commit = format!(
            "  nothing was recorded; record a commit that carries that fix; {keep}, {allow}"
        );
        for (lines, case) in [
            (&mixed, "mixed"),
            (&changed, "changed"),
            (&beside, "beside"),
        ] {
            assert_eq!(closing(lines), Some(&commit), "{case}");
        }
    }

    /// The gate's lines for a record of `release/2026-11-14` at `eeee` that
    /// lacks `fixes`, held against the newest write, `release/2026-11-13`'s
    /// cut `bbbb`, where the fork's remotes have what `remotes` has. Each
    /// line is escaped.
    fn fix_refusal(fixes: Vec<LackedFix>, remotes: &Remotes<'_>) -> Vec<String> {
        let write = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-11-13".to_owned(),
            commit: CommitId::new("bbbb"),
            members: Vec::new(),
        };
        let at = CommitId::new("eeee");
        let held = HeldRecords {
            first: HeldRecord::NewestWrite(&write),
            other: None,
            record: Some(("release/2026-11-14", &at)),
            past: None,
        };
        let check = CompositionCheck {
            dropped_fixes: fixes,
            ..CompositionCheck::default()
        };
        let lines = uncarried_cut_lines(
            &RepoName::new("demo"),
            held,
            &check,
            (DropDisposition::RefuseRecordingNothing, remotes),
        );
        assert_escaped(&lines);
        lines
    }

    #[test]
    fn a_cut_event_names_a_dropped_member_that_only_the_newest_write_held_with_it() {
        // Given: a cut of alpha and gamma, then an edit that added beta; the
        // cut that follows carries alpha alone.
        let (alpha, beta, gamma) = (
            CommitId::new("aaaa"),
            CommitId::new("bbbb"),
            CommitId::new("cccc"),
        );
        let cut = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("dddd"),
            members: vec![alpha.clone(), gamma.clone()],
        };
        let write = RecordedCut {
            kind: RecordedWriteKind::Edit,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("eeee"),
            members: vec![alpha, gamma.clone(), beta.clone()],
        };
        let check = CompositionCheck {
            dropped: vec![
                RecordedMember {
                    commit: gamma,
                    name: "feat/gamma@cccc".to_owned(),
                },
                RecordedMember {
                    commit: beta,
                    name: "feat/beta@bbbb".to_owned(),
                },
            ],
            ..CompositionCheck::default()
        };
        let held = HeldRecords {
            first: HeldRecord::PreviousCut(&cut),
            other: Some(HeldRecord::NewestWrite(&write)),
            record: None,
            past: None,
        };

        // When: its event restates the records it was held against.
        let delta = restated_delta(Some(held), &check);

        // Then: the previous cut's member is dropped as a cut has always said,
        // and the member only the edit held is named with that write.
        assert_eq!(
            delta,
            "; previous cut release/2026-08-04 recorded 2 member(s); dropped: feat/gamma@cccc; \
             dropped from release/2026-08-04's newest recorded write (an edit): feat/beta@bbbb"
        );
        assert_eq!(restated_delta(None, &check), "");
    }

    #[test]
    fn a_cut_event_names_each_file_of_a_fix_it_went_without() {
        // Given: a cut that `--allow-drop` let go ahead without the fix by
        // hand of an adopt at two of its files, one a path that holds `, `.
        let cut = RecordedCut {
            kind: RecordedWriteKind::Cut,
            name: "release/2026-08-04".to_owned(),
            commit: CommitId::new("dddd"),
            members: Vec::new(),
        };
        let check = CompositionCheck {
            dropped_fixes: vec![LackedFix {
                name: "release/2026-08-04".to_owned(),
                commit: CommitId::new("aaaa"),
                files: vec![
                    held_file("x, y.txt", FixReference::Conflicted),
                    held_file("z.txt", FixReference::Conflicted),
                ],
            }],
            absent_adopts: vec![
                AbsentAdopt {
                    name: "release/2026-08-03".to_owned(),
                    commit: CommitId::new("9b960c645167"),
                },
                AbsentAdopt {
                    name: "release/2026-08-04".to_owned(),
                    commit: CommitId::new("e1af5dbd524a"),
                },
            ],
            ..CompositionCheck::default()
        };
        let held = HeldRecords {
            first: HeldRecord::PreviousCut(&cut),
            other: None,
            record: None,
            past: None,
        };

        // When: its event restates the records it was held against.
        let delta = restated_delta(Some(held), &check);

        // Then: it names the adopt and each file it went without, raw, then
        // each adopt this repository does not hold.
        assert_eq!(
            delta,
            "; previous cut release/2026-08-04 recorded 0 member(s); all carried; without the fix \
             by hand of release/2026-08-04's adopt aaaa, in 2 file(s): x, y.txt, z.txt; not held \
             against 2 adopt(s) this repository does not hold: 9b960c645167, e1af5dbd524a"
        );
    }

    #[test]
    fn the_fix_of_an_adopt_from_the_ledger_reaches_the_terminal_escaped() {
        // Given: an adopt's release and commit, which are a ledger entry's
        // subject and first evidence string, and a file of its fix, a path
        // read from the repository.
        let (name, commit) = (format!("release/{HOSTILE}"), CommitId::new(HOSTILE));

        // When: a message names the fix, and the cut event names it raw.
        let shown = format!(
            "{}{}",
            fix_title((&name, &commit), inline_human_text),
            fix_files(
                &[held_file(
                    &format!("fix{HOSTILE}.txt"),
                    FixReference::Conflicted,
                )],
                inline_human_text
            )
        );
        let raw = fix_title((&name, &commit), str::to_owned);

        // Then: the message is escaped, names the adopt and counts the file;
        // the event keeps the text as the ledger has it.
        assert_escaped(std::slice::from_ref(&shown));
        assert_eq!(
            shown,
            format!(
                "the fix by hand of release/{ESCAPED}'s adopt {}, in \
                 1 file(s): fix{ESCAPED}.txt",
                ids_prefix(ESCAPED)
            )
        );
        assert_eq!(
            raw,
            format!(
                "the fix by hand of release/{HOSTILE}'s adopt {}",
                commit.short()
            )
        );
    }

    #[test]
    fn allow_drop_names_a_commit_or_a_file_and_counts_each_value_it_used() {
        // Given: a commit by its short form, a path, a path of one adopt,
        // and a value shorter than a short commit.
        let allow = AllowDrop::new(vec![
            "0123456789ab".to_owned(),
            "fix.txt".to_owned(),
            "aaaaaaaaaaaa:other.txt".to_owned(),
            "0123".to_owned(),
        ]);
        let (adopt, other_adopt) = (
            CommitId::new("aaaaaaaaaaaaaaaaaaaa"),
            CommitId::new("bbbbbbbbbbbbbbbbbbbb"),
        );

        // Then: the commit is named by its short form, the path for any
        // adopt, the adopt's path for that adopt alone, and the short value
        // names nothing, so it is unused and the write is refused.
        assert!(allow.commit(&CommitId::new("0123456789abcdef0123456789abcdef01234567")));
        assert!(allow.file(&other_adopt, "fix.txt"));
        assert!(allow.file(&adopt, "other.txt"));
        assert!(!allow.file(&other_adopt, "other.txt"));
        assert!(!allow.commit(&CommitId::new("0123ffffffffffffffff")));
        assert_eq!(allow.settle(&RepoName::new("demo")), Some(Exit::Incomplete));

        // And: values that each named a drop leave nothing unused.
        let used = AllowDrop::new(vec!["fix.txt".to_owned()]);
        assert!(used.file(&adopt, "fix.txt"));
        assert_eq!(used.settle(&RepoName::new("demo")), None);
    }

    /// A file of a fix by hand at `path`, held against `reference`, that
    /// the adopt's change does not change.
    fn held_file(path: &str, reference: FixReference) -> HeldFile {
        HeldFile {
            path: path.to_owned(),
            reference,
            replayed: false,
        }
    }

    /// What `short` keeps of an escaped id: its first twelve characters.
    fn ids_prefix(text: &str) -> String {
        text.chars().take(12).collect()
    }

    #[test]
    fn a_workspace_name_with_a_control_character_reaches_the_terminal_escaped() {
        // `jj workspace add --name` accepts ESC and C1 in a name.
        let lines = orphan_workspace_lines(&["plain".to_owned(), format!("ws{HOSTILE}")]);

        assert_escaped(&lines);
        assert!(
            lines
                .first()
                .is_some_and(|line| line.ends_with(&format!("no longer exist: plain, ws{ESCAPED}"))),
            "{lines:?}"
        );
    }

    #[test]
    fn a_reap_names_its_cuts_and_their_keepers_escaped() {
        let report = ReapReport {
            reaped: vec![format!("release/{HOSTILE}")],
            forgotten_only: vec![(
                "release/2026-01-01".to_owned(),
                format!("aaaa still pinned by bookmark {HOSTILE}"),
            )],
            kept: vec![(
                "release/2026-01-02".to_owned(),
                format!("the working copy of workspace ws{HOSTILE} rests on it"),
            )],
            kept_published: Vec::new(),
            notes: vec![format!(
                "release/2026-01-03: refs forgotten: jj said\nError: {HOSTILE}"
            )],
            hidden: Vec::new(),
        };

        let lines = reap_lines(&RepoName::new("demo"), &report);

        assert_escaped(&lines);
        let text = lines.join("\n");
        assert!(
            text.contains(&format!("reaped release/{ESCAPED} (refs")),
            "{text:?}"
        );
        assert!(text.contains(&format!("bookmark {ESCAPED})")), "{text:?}");
        assert!(
            text.contains(&format!(
                "kept release/2026-01-02: the working copy of workspace ws{ESCAPED}"
            )),
            "{text:?}"
        );
        assert!(
            text.contains(&format!("jj said; Error: {ESCAPED}")),
            "{text:?}"
        );
    }
}
