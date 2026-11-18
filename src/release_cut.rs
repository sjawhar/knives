//! `knives release` (plan and cut) and `knives release reap`.
//!
//! A cut is one merge of the carried members, gated on what the previous cut
//! recorded, what the working copies hold, and what would be orphaned; the
//! ledger event and the superseded-cut reap follow it. Rebasing the
//! composition lives in `release_rebase`; the membership verbs in
//! `release_edit`.

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::commands::release;
use knives::forge::github::CliForge;
use knives::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName, RepoName};
use knives::ledger::{Draft, Ledger, inline_human_text, problem_human_text, shell_word};
use knives::release_model::{
    CompositionBaseline, EarlierWrites, PreviousRelease, RecordedCut, RecordedWriteKind,
    StackedHistoryContext, carried_branches, members_event_text, previous_release_for_cut,
    previous_release_for_record, previous_release_label, series_members, trunk_positions,
    trunk_positions_in, trunk_reaches,
};

use super::release_edit::{
    announce_unchecked, composition_sources, provenance_parents, report_hidden, union_event_text,
};
use super::scribe_for;

pub(crate) enum ReleaseInvocation {
    Plan,
    Cut {
        name: Option<String>,
        allow_drop: bool,
        /// The commit `--at` records `name` at: a release cut outside knives.
        at: Option<String>,
    },
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
    let opened = knives::jj::Repo::open(path)?;
    let scheme = entry.release_scheme();
    let (cut_name, allow_drop, at) = match requested_cut(invocation, &scheme, entry) {
        Ok(request) => request,
        Err(exit) => return Ok(exit),
    };
    let (mut worst, pins) = release_plan_exit(
        fork,
        bound.registry,
        &locals,
        &opened,
        &forge,
        cache_root.as_deref(),
        &heads,
    )?;
    if worst == Exit::Incomplete {
        return Ok(worst);
    }

    if let Some(name) = cut_name {
        // A release cut outside knives is recorded, never composed: no orphan
        // gate, no candidate, no reap.
        if let Some(at) = at {
            return Ok(worst.worst(record_hand_cut(
                fork,
                &opened,
                (&name, at, allow_drop),
                bound,
            )?));
        }
        let trunk_name = entry.trunk_ref();
        let trunk = opened.resolve_commit(&trunk_name)?;
        let orphaned = check_orphan_commits_before_cut(&opened, fork)?;
        if let Some(orphaned) = &orphaned
            && let Some(exit) = report_orphaned_cut(repo, orphaned, allow_drop)
        {
            return Ok(exit);
        }
        let tips = opened.bookmark_tips()?;
        let previous = previous_release_for_cut(entry, &tips);
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
        let ledger = Ledger::for_fork(fork).read()?.entries;
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
                    tips: &tips,
                };
                let standing = release::in_hand_write(&in_hand, entry)?;
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
        // A stacked cut starts as one commit on the previous release, with
        // its tree: identical to it, so it is allowed only where a verbatim
        // new name is (below), and that is decided before anything is built.
        if entry.stacked()
            && let Some((previous_ref, previous_tip)) = &previous
            && let Some(exit) = stacked_start_gate(
                &opened,
                (repo, &name, &pins),
                (&scheme, &ledger),
                (previous_ref, previous_tip, previous_published),
            )?
        {
            return Ok(exit);
        }
        let (carried, members, audit_base) = if let Some((previous_ref, previous)) = &previous {
            if entry.stacked() {
                // The previous release is the stacked cut's one parent and
                // never a member: nothing is replayed, and every member
                // `include` adds later is measured from it.
                let labelled = (
                    previous_release_label(previous_ref.branch().as_str()),
                    previous.clone(),
                );
                (vec![labelled], Vec::new(), previous.clone())
            } else {
                let carried = release::parent_sources(&opened, entry, &scheme, &previous_members)?;
                let members = carried.clone();
                let base = release::shared_base(&opened, previous, &trunk)?
                    .unwrap_or_else(|| trunk.clone());
                (carried, members, base)
            }
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
            let members = carried.clone();
            // The first cut audits each branch from the fork point too:
            // measuring from the trunk tip charges every commit upstream
            // landed since the fork to the branches themselves.
            let member_tips: Vec<knives::ids::CommitId> =
                carried.iter().map(|(_, tip)| tip.clone()).collect();
            let base = opened.fork_point_of(&member_tips, &trunk)?;
            (carried, members, base)
        };
        let request = release::Cut::from_carried(name.clone(), &carried);
        // A stacked cut is a fresh merge of its one parent, the previous
        // release; any other duplicates the previous release onto its parents.
        let mut candidate = release::candidate_cut(
            path,
            &request,
            previous_commit
                .as_ref()
                .filter(|_| !entry.stacked())
                .map(|previous| (previous, previous_carriers.as_slice())),
        )?;
        // The orphan gate protects unreachable commits; this protects the
        // composition itself. The ledger's events are the only record of a
        // parent set that survives the bookmark moving, so every subject is
        // held against the previous cut's event and the newest recorded write
        // of the newest release the ledger records before anything is
        // recorded or published. In a stacked series that write must also be
        // on the line of the commit the cut starts on, its first parent. Each
        // fix by hand that the series holds, an `adopt` of any of its
        // releases whose fix that write still carries, is held as a member is.
        let (baseline, fixes) =
            CompositionGate::read_baseline((&opened, entry), (&ledger, &tips), None)?;
        let gate = CompositionGate {
            baseline,
            record: None,
            opened: &opened,
            path,
            parents: &request.parents,
            base: &audit_base,
            trunk: &trunk,
            tips: &tips,
            start: request.parents.first().filter(|_| entry.stacked()),
            fixes,
        };
        // A cut is audited against the release it duplicates: a member's
        // divergence that release already had is carried, and the drift is
        // measured from it. A stacked cut starts with no member.
        let context = release::AuditContext {
            previous: previous_commit.as_ref(),
            trunk: &audit_base,
            stacked_on: None,
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
        if let Some((previous_ref, previous_tip)) = &previous {
            let publish_remote = entry.publish_remote();
            let published = tips.get(&BookmarkRef::Remote {
                branch: previous_ref.branch().clone(),
                remote: RemoteName::new(publish_remote),
            });
            if let Some(published) = published
                && candidate.matches(published.as_str())?
            {
                let why = verbatim_cut_reason(
                    &opened,
                    (&pins, &scheme, &ledger),
                    (previous_ref, previous_tip, Some(published)),
                )?;
                // The previous release is a name read from the repository, and
                // the evidence names a tag on it.
                let shown_previous = inline_human_text(previous_ref.branch().as_str());
                if let Some(why) = why {
                    println!(
                        "{repo}: {name} starts identical to {shown_previous}@{publish_remote} ({}); {why}, so this new name is the composition to edit — `include`, `advance`, `drop` or `rebase` it before pushing",
                        published.short()
                    );
                } else if matches!(scheme, ReleaseScheme::Fixed(_))
                    && gate
                        .baseline
                        .previous_cut
                        .as_ref()
                        .is_none_or(|recorded| recorded.commit != *published)
                {
                    let committed = CommittedCut {
                        commit: published,
                        members: &members,
                        context,
                        carried_from,
                    };
                    let checked =
                        match checked_committed_cut(repo, &committed, (&gate, allow_drop))? {
                            Ok(checked) => checked,
                            Err(exit) => return Ok(exit),
                        };
                    let (audit, check) = (&checked.audit, &checked.check);
                    let completed = CompletedCut {
                        name: &name,
                        request: &request,
                        created: published,
                        audit,
                        scheme: &scheme,
                        held: gate.held(),
                        check,
                        stated_drop: None,
                        // Recorded as published, not written: knives unioned nothing.
                        unions: &[],
                        unchecked: &unchecked,
                        composition: None,
                    };
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
                    if let Some(orphaned) = &orphaned {
                        report_stated_drop(fork, orphaned)?;
                    }
                    if !audit.inconclusive.is_empty() || !check.inconclusive.is_empty() {
                        worst = worst.worst(Exit::Findings);
                    }
                    return Ok(worst);
                } else {
                    println!(
                        "{repo}: refusing to cut {name}: identical to {shown_previous}@{publish_remote} ({}); nothing to cut",
                        published.short()
                    );
                    return Ok(Exit::Incomplete);
                }
            }
        }
        // An audit error or failure simply DROPS the candidate: the merge
        // was never a published operation, so there is nothing to abandon
        // and no crash window that strands one. A file of a fix by hand that
        // the gate holds is the gate's to judge, not the drift check's.
        let mut audit = release::audit_cut(
            path,
            &members,
            release::CutSubject::Candidate(&mut candidate),
            context,
        )?;
        audit.leave_fixes_to_gate(&gate.fixes);
        if let Some(exit) = report_cut_audit(repo, &audit, carried_from, context.drift_from) {
            return Ok(exit);
        }
        let check = {
            let mut subject = release::CutSubject::Candidate(&mut candidate);
            match recorded_composition_check(repo, &mut subject, &gate, allow_drop)? {
                Ok(check) => check,
                Err(exit) => return Ok(exit),
            }
        };
        // The publish rebuilds the audited tree, so the candidate's unions are
        // what ships.
        let unions = candidate.unions().to_vec();
        let published = release::publish_cut(
            candidate,
            &request.name,
            &scheme,
            (previous_writes.commits(), Some(entry.publish_remote())),
        )?;
        let created = published.commit;
        let completed = CompletedCut {
            name: &name,
            request: &request,
            created: &created,
            audit: &audit,
            scheme: &scheme,
            held: gate.held(),
            check: &check,
            stated_drop: orphaned.as_ref(),
            unions: &unions,
            unchecked: &unchecked,
            composition: None,
        };
        record_cut_event(fork, &completed, bound)?;
        worst = worst.worst(report_completed_cut(fork, &opened, &completed, bound)?);
        report_hidden(
            repo,
            previous
                .as_ref()
                .map_or(name.as_str(), |(reference, _)| reference.branch().as_str()),
            &published.hidden,
        );
    }
    Ok(worst)
}

/// The name a cut takes, `--allow-drop`, and the commit `--at` records the
/// name at, or the usage error's exit after saying why.
fn requested_cut<'a>(
    invocation: &'a ReleaseInvocation,
    scheme: &knives::ids::ReleaseScheme,
    entry: &knives::config::RepoEntry,
) -> Result<(Option<String>, bool, Option<&'a str>), Exit> {
    match invocation {
        ReleaseInvocation::Plan => Ok((None, false, None)),
        ReleaseInvocation::Cut {
            name,
            allow_drop,
            at,
        } => {
            // A fixed branch has a recording path of its own.
            if at.is_some()
                && let ReleaseScheme::Fixed(fixed) = scheme
            {
                eprintln!(
                    "--at records a dated release cut outside knives, and this repo releases on \
                     the fixed branch {fixed}: a plain `knives release cut` records {fixed}@{} as \
                     its cut when that ref holds the composition in hand and no cut records it",
                    entry.publish_remote()
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
                Ok(name) => Ok((Some(name), *allow_drop, at.as_deref())),
                Err(message) => {
                    eprintln!("{message}");
                    Err(Exit::Usage)
                }
            }
        }
    }
}

/// The plan's exit, and the consumer pins it scanned. The cut judges the pins
/// itself, against the release it compares the candidate with: when every pin
/// of that release is frozen, a new dated name is the only editable
/// composition, so the identical-composition refusal must stand down for it.
#[allow(
    clippy::too_many_arguments,
    reason = "the release plan needs explicit repository state and each independently owned consumer-scan collaborator"
)]
fn release_plan_exit(
    fork: &Fork<'_>,
    registry: &knives::config::Registry,
    locals: &[std::path::PathBuf],
    opened: &knives::jj::Repo,
    forge: &dyn knives::consumer_pins::ConsumerPinSource,
    cache_root: Option<&std::path::Path>,
    heads: &knives::consumer_pins::ConsumerHeadMemo,
) -> anyhow::Result<(Exit, Vec<knives::pins::Pin>)> {
    let entry = fork.entry;
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals,
        forge,
        cache_root,
        heads,
    };
    let plan = release::pulled_plan(fork, registry, &consumers)?;
    println!("{}", release::render(&plan));
    let mut exit = release::exit_for(&plan);
    if let Some(lag) = release::trunk_lag(opened, plan.release.as_deref(), &entry.trunk_ref()) {
        // The line names the release, a ref read from the repository.
        println!("  !! {}", inline_human_text(&lag));
        exit = exit.worst(Exit::Findings);
    }
    Ok((exit, plan.pins))
}

/// The gate of a cut in a stacked series that has a previous release, before
/// anything is built: `Some` after refusing, `None` after saying why the cut
/// may go ahead.
///
/// Such a cut starts as one commit on the previous release, with its tree,
/// and becomes the next release as members are included into it. So it
/// always starts identical to the previous release, and the identical gate
/// (`Candidate::matches`) cannot see it: that gate compares parents too, and
/// the start's one parent is the previous release itself. The start is
/// allowed where a verbatim cut under a new dated name is
/// ([`verbatim_cut_reason`]). Otherwise there is nothing to cut, and the
/// previous release is the composition to edit.
fn stacked_start_gate(
    opened: &knives::jj::Repo,
    (repo, name, pins): (&RepoName, &str, &[knives::pins::Pin]),
    (scheme, ledger): (&ReleaseScheme, &[knives::ledger::Entry]),
    (previous, tip, published): (&BookmarkRef, &CommitId, Option<&CommitId>),
) -> anyhow::Result<Option<Exit>> {
    // The previous release is a name read from the repository.
    let shown = inline_human_text(previous.branch().as_str());
    let Some(why) =
        verbatim_cut_reason(opened, (pins, scheme, ledger), (previous, tip, published))?
    else {
        println!(
            "{repo}: refusing to cut {name}: it would start identical to {shown} ({}), as one \
             commit on it; nothing to cut: {shown} is neither published (no tag or `record: \
             published` note names it) nor frozen by every pin, so `knives release include \
             <branch>` edits {shown} itself",
            tip.short()
        );
        return Ok(Some(Exit::Incomplete));
    };
    println!(
        "{repo}: {name} starts identical to {shown} ({}), as one commit on it; {why}, so this \
         new name is the composition to edit — `include`, `advance` or `drop` it before pushing",
        tip.short()
    );
    Ok(None)
}

/// Why a cut under a new dated name may start identical to `previous`, at
/// `tip` here and `published` on the publish remote, or `None`: every pin of
/// a dated or series release is frozen, or it is published (a tag or a
/// `record: published` note names its commit), so it cannot be edited in
/// place. The identical gate and [`stacked_start_gate`] each say so in their
/// own words. Both facts are read whichever one holds.
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
/// Inconclusive members are reported without failing the cut: a conflicted
/// replay onto a conflicted cut answers nothing either way. Missing or
/// unexplained content refuses the cut, because naming it would look exactly
/// like success while work is gone. The refused candidate was never a
/// published operation, so nothing is abandoned and nothing needs cleanup.
/// A divergence carried from the release in hand reads by how that release
/// stands to its recorded writes (`carried_from`), and an unexplained file
/// names what the drift was measured from (`drift_from`, the audit's
/// [`release::AuditContext::drift_from`]).
fn report_cut_audit(
    repo: &RepoName,
    audit: &release::CutAudit,
    carried_from: release::CarriedFrom,
    drift_from: Option<(&CommitId, release::DriftBase)>,
) -> Option<Exit> {
    for name in &audit.carried {
        println!("{}", release::carried_row(name, carried_from));
    }
    // Each name is a member's bookmark, read from the repository.
    for name in &audit.inconclusive {
        println!(
            "  {}: content check inconclusive (replay conflicted; \
             re-check after resolving the cut's conflicts)",
            inline_human_text(name)
        );
    }
    if audit.passed() {
        return None;
    }
    for name in &audit.missing {
        println!(
            "  !! {}: the cut tree is missing or diverges from the member's content",
            inline_human_text(name)
        );
    }
    // Only a drift check finds a file no member explains.
    if let Some((_, base)) = drift_from {
        for file in &audit.unexplained {
            println!("{}", release::unexplained_row(file, base));
        }
    }
    println!("{repo}: cut discarded; nothing was written at all. Fix the inputs and re-cut.");
    Some(Exit::Incomplete)
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
    path: &'a std::path::Path,
    parents: &'a [knives::ids::CommitId],
    base: &'a knives::ids::CommitId,
    trunk: &'a knives::ids::CommitId,
    tips: &'a knives::detect::BookmarkTips,
    /// In a stacked series, the commit the subject starts on, whose
    /// first-parent line must hold the newest recorded write
    /// ([`release::CompositionDelta::start`]): the commit a record names, or
    /// the previous release a cut starts on. `None` in any other series.
    start: Option<&'a CommitId>,
    /// The fixes by hand that the series holds: each `adopt` of any of its
    /// releases whose fix the newest recorded write still carries
    /// ([`release::adopted_fixes`]).
    fixes: Vec<release::AdoptedFix>,
}

impl CompositionGate<'_> {
    /// The baseline of the gate of `record` ([`Self::record`]), read from
    /// `ledger` in `entry`'s series: the name a record records is aside, as
    /// no write of it is its own baseline. With it, the fixes by hand that
    /// the series holds ([`release::adopted_fixes`]), read in `opened` with
    /// `tips`.
    fn read_baseline(
        (opened, entry): (&knives::jj::Repo, &knives::config::RepoEntry),
        (ledger, tips): (&[knives::ledger::Entry], &knives::detect::BookmarkTips),
        record: Option<(&str, &CommitId)>,
    ) -> anyhow::Result<(CompositionBaseline, Vec<release::AdoptedFix>)> {
        let baseline = CompositionBaseline::read(ledger, entry, record.map(|(name, _)| name));
        let newest = baseline.newest_write.as_ref();
        let fixes = release::adopted_fixes(opened, entry, (ledger, tips), newest)?;
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
            fixes: &self.fixes,
        })
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
    /// The fixes by hand of the newest release ([`CompositionGate::fixes`]).
    fixes: &'a [release::AdoptedFix],
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
}

/// Hold the cut subject against the gate's [`CompositionBaseline`]: each
/// member the previous cut's event or the newest write records, and in a
/// stacked series the newest write itself, on the line of the commit the
/// subject starts on ([`CompositionGate::start`]).
///
/// `Err` is the refusal, already reported. `Ok` carries what the check
/// found, for the cut event to restate.
fn recorded_composition_check(
    repo: &RepoName,
    subject: &mut release::CutSubject<'_>,
    gate: &CompositionGate<'_>,
    allow_drop: bool,
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
            start: gate.start,
            fixes: &gate.fixes,
        },
    )?;
    let on_drop = match (allow_drop, &*subject) {
        (true, _) => DropDisposition::Allowed,
        (false, release::CutSubject::Candidate(_)) => DropDisposition::RefuseDiscardingCandidate,
        (false, release::CutSubject::Committed(_)) => DropDisposition::RefuseRecordingNothing,
    };
    if let Some(exit) = report_uncarried_cut(repo, held, &check, on_drop) {
        return Ok(Err(exit));
    }
    Ok(Ok(check))
}

/// What a cut does about recorded members that it lacks.
#[derive(Clone, Copy)]
enum DropDisposition {
    /// `--allow-drop`: state the drop and carry on.
    Allowed,
    /// Refuse; the candidate commit was never published, so it is simply gone.
    RefuseDiscardingCandidate,
    /// Refuse; the composition is already on origin, so nothing changes there.
    RefuseRecordingNothing,
}

/// Say what the recorded-composition check found; refuse when members are
/// gone, when a stacked subject is not built on the newest write, or when
/// the subject lacks a fix by hand that the series holds: an `adopt` of any
/// of its releases whose fix the newest write still carries.
///
/// Inconclusive members are reported without refusing, for the same reason the
/// audit's are: a conflicted replay onto a conflicted cut subject answers
/// nothing either way. A member the cut subject does not carry refuses the cut,
/// the recorded writes' ledger events are the only surviving record of the
/// composition — every edit moves the bookmark and abandons the commit it
/// replaced. A refused candidate was never published and a refused
/// record leaves origin as it was, so nothing is abandoned and nothing needs
/// cleanup.
fn report_uncarried_cut(
    repo: &RepoName,
    held: HeldRecords<'_>,
    check: &release::CompositionCheck,
    on_drop: DropDisposition,
) -> Option<Exit> {
    for line in uncarried_cut_lines(repo, held, check, on_drop) {
        println!("{line}");
    }
    ((!check.dropped.is_empty() || check.off_line || !check.dropped_fixes.is_empty())
        && !matches!(on_drop, DropDisposition::Allowed))
    .then_some(Exit::Incomplete)
}

/// The lines [`report_uncarried_cut`] prints, which name each member with
/// the record that names it ([`HeldRecords::holder`]), the newest write a
/// stacked subject is not built on, and each adopt whose fix the subject
/// lacks ([`fix_title`]).
///
/// The recorded release's name and its members are a ledger entry's subject
/// and evidence, which a peer's pulled ledger can carry any text in; a member
/// is rendered with the bookmark that holds it, a name read from the
/// repository. Each is escaped here; the cut event keeps them raw.
fn uncarried_cut_lines(
    repo: &RepoName,
    held: HeldRecords<'_>,
    check: &release::CompositionCheck,
    on_drop: DropDisposition,
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
            "  {}: recorded by the {}; carry check inconclusive (replay conflicted; re-check \
             after resolving the cut's conflicts)",
            inline_human_text(&member.name),
            holder.title(&inline_human_text(&holder.recorded().name))
        ));
    }
    let off_line = held.newest_write().filter(|_| check.off_line);
    let fixes: Vec<&release::AdoptedFix> = held
        .fixes
        .iter()
        .filter(|fix| check.dropped_fixes.contains(&fix.commit))
        .collect();
    if check.dropped.is_empty() && off_line.is_none() && fixes.is_empty() {
        return lines;
    }
    let allowed = matches!(on_drop, DropDisposition::Allowed);
    let (first, other) = held.split(&check.dropped);
    let mut sources = Vec::new();
    for (source, dropped) in std::iter::once((held.first, first)).chain(other) {
        if !dropped.is_empty() {
            lines.extend(dropped_lines(repo, (held, source), &dropped, allowed));
            sources.push(source);
        }
    }
    if let Some(write) = off_line {
        let named = write.with_commit(inline_human_text);
        lines.push(if allowed {
            format!(
                "{repo}: --allow-drop: {} without {named} on its first-parent line",
                held.doing()
            )
        } else {
            format!(
                "{} does not have {named} on its first-parent line, so it does not carry {}",
                held.refusing(repo),
                inline_human_text(&write.recorded().name)
            )
        });
    }
    for fix in &fixes {
        let named = format!(
            "{}{}",
            fix_title((&fix.name, &fix.commit), inline_human_text),
            fix_files(&fix.files)
        );
        lines.push(if allowed {
            format!("{repo}: --allow-drop: {} without {named}", held.doing())
        } else {
            format!("{} does not carry {named}", held.refusing(repo))
        });
    }
    if !allowed {
        lines.push(refusal_remedy(
            held,
            (&sources, off_line, fixes.len()),
            on_drop,
        ));
    }
    lines
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

/// What a message says after [`fix_title`]: `, in N file(s): F, G`, the
/// fix's files, which are the repository's paths, escaped; or, with none,
/// that nothing here can measure the fix ([`release::AdoptedFix::files`]).
fn fix_files(files: &[String]) -> String {
    if files.is_empty() {
        return " (nothing here can measure it: this repository does not hold the adopt or \
                cannot read its release)"
            .to_owned();
    }
    let shown: Vec<String> = files.iter().map(|file| inline_human_text(file)).collect();
    format!(", in {} file(s): {}", shown.len(), shown.join(", "))
}

/// The line that closes a refusal of [`uncarried_cut_lines`]: what each
/// refused finding asks for, or `--allow-drop`. `sources` are the records
/// whose members the subject lacks, `off_line` the newest write a stacked
/// subject is not built on, and `fixes` how many fixes by hand that the
/// series holds the subject lacks.
///
/// A record is built again: on that write when it is off the line, which
/// carries its fixes too. A cut gets back a member of the previous cut with
/// `include`. A member only the newest write holds, a write off the cut's
/// line, and a fix that the newest write carries and the cut lacks mean the
/// release here is older than that write, as on a machine that has not
/// fetched it: so the cut is made again once that write's release is moved
/// to it, and an `include` on the older commit would write a second,
/// divergent write.
fn refusal_remedy(
    held: HeldRecords<'_>,
    (sources, off_line, fixes): (&[HeldRecord<'_>], Option<HeldRecord<'_>>, usize),
    on_drop: DropDisposition,
) -> String {
    const ALLOW: &str = "or re-run with --allow-drop to state the drop is intended";
    if held.record.is_some() {
        let remedy = match off_line {
            Some(write) if sources.is_empty() => format!(
                "build the commit on {} and record that one",
                inline_human_text(write.recorded().commit.short())
            ),
            Some(write) => format!(
                "record a commit built on {} that carries each of them",
                inline_human_text(write.recorded().commit.short())
            ),
            None if sources.is_empty() && fixes == 1 => {
                "record a commit that carries that fix".to_owned()
            }
            None if sources.is_empty() => "record a commit that carries those fixes".to_owned(),
            None if fixes > 0 => {
                "record a commit that carries each of them and each fix".to_owned()
            }
            None => "record a commit that carries each of them".to_owned(),
        };
        return format!("  nothing was recorded; {remedy}, {ALLOW}");
    }
    let outcome = match on_drop {
        DropDisposition::RefuseDiscardingCandidate => "the candidate was discarded",
        DropDisposition::Allowed | DropDisposition::RefuseRecordingNothing => {
            "nothing was recorded"
        }
    };
    let mut remedies = Vec::new();
    if sources
        .iter()
        .any(|source| matches!(source, HeldRecord::PreviousCut(_)))
    {
        remedies.push(
            "`knives release include <branch>` restores a member the previous cut recorded"
                .to_owned(),
        );
    }
    let newer = sources
        .iter()
        .copied()
        .find(|source| matches!(source, HeldRecord::NewestWrite(_)))
        .or(off_line)
        .or_else(|| held.newest_write().filter(|_| fixes > 0));
    if let Some(write) = newer {
        let recorded = write.recorded();
        let (name, commit) = (&recorded.name, recorded.commit.short());
        remedies.push(format!(
            "fetch, then move {} to {}, its newest recorded write: `jj bookmark set {} -r {} \
             --allow-backwards`",
            inline_human_text(name),
            inline_human_text(commit),
            shell_word(name),
            shell_word(commit)
        ));
    }
    format!("  {outcome}; {}, {ALLOW}", remedies.join("; "))
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
    audit: &'a release::CutAudit,
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
    /// --at` records: its previous release first, under its label, when it
    /// has one, then its members, every parent the trunk holds left out
    /// ([`record_hand_cut`]). The event records it, and says the release was
    /// composed outside knives ([`RECORDED_OUTSIDE`]). `None` for a cut
    /// knives wrote or recorded as published, whose event reads its
    /// composition from the commit ([`cut_composition`]).
    composition: Option<&'a [(String, CommitId)]>,
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
/// [`cut_composition`]).
fn record_cut_event(
    fork: &Fork<'_>,
    cut: &CompletedCut<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    let entry = fork.entry;
    let opened = knives::jj::Repo::open(&fork.checkout.path)?;
    let change = opened.change_id_of(cut.created.as_str())?;
    let members: std::borrow::Cow<'_, [(String, CommitId)]> = match cut.composition {
        Some(composition) => std::borrow::Cow::Borrowed(composition),
        None => std::borrow::Cow::Owned(cut_composition(
            fork,
            &opened,
            (cut.name, cut.created),
            cut.scheme,
        )?),
    };
    let members_text = members_event_text(&members);
    let mut evidence = vec![cut.created.as_str().to_owned()];
    evidence.extend(members.iter().map(|(_, commit)| commit.as_str().to_owned()));
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
    let delta = restated_delta(cut.held, cut.check);
    scribe_for(fork, bound)?.record(&Draft {
        evidence,
        parents: provenance_parents(&opened, entry, &members)?,
        ..Draft::event(
            Some(cut.name),
            format!(
                "cut {} as {} (change {}) with {} parent(s): {members_text}{delta}{}{}{}",
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

/// What a cut event says after its members about the records the cut was
/// held against ([`CompositionGate::held`]): the first record as what it is
/// and how many members it holds ([`HeldRecord::restated`]), `; previous cut
/// NAME recorded N member(s)` or `; NAME's newest recorded write (an edit)
/// holds N member(s)`, then `all carried` or the members the subject
/// dropped, each member that only the other record holds named with that
/// record, then `; not built on <the newest write> <commit>` for a stacked
/// subject that `--allow-drop` let off that write's line, then `; without
/// the fix by hand of <release>'s adopt <commit>` for each fix by hand that
/// `--allow-drop` let the subject go ahead without ([`fix_title`]), then the
/// unverified members. Empty when the ledger
/// records neither. The event keeps each name raw.
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
    let off_line = held
        .newest_write()
        .filter(|_| check.off_line)
        .map_or_else(String::new, |write| {
            format!("; not built on {}", write.with_commit(str::to_owned))
        });
    let fixes = held
        .fixes
        .iter()
        .filter(|fix| check.dropped_fixes.contains(&fix.commit))
        .fold(String::new(), |mut text, fix| {
            text.push_str("; without ");
            text.push_str(&fix_title((&fix.name, &fix.commit), str::to_owned));
            text
        });
    let unverified = if check.inconclusive.is_empty() {
        String::new()
    } else {
        format!(
            "; unverified: {}",
            check
                .inconclusive
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "; {}; {carried}{off_line}{fixes}{unverified}",
        held.first.restated()
    )
}

/// What a cut event records of `created`, the commit cut as `name`: each
/// parent with where it came from ([`release::parent_sources`]), in parent
/// order.
///
/// A release of a stacked series records its reading instead
/// ([`release::release_reading`]): its previous release first when it has
/// one, under its label ([`PreviousRelease::labelled`]), then its members,
/// the side parents on its first-parent line, never the commit's literal
/// parents. A commit the reading refuses is no release to record.
fn cut_composition(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    (name, created): (&str, &CommitId),
    scheme: &ReleaseScheme,
) -> anyhow::Result<Vec<(String, CommitId)>> {
    let entry = fork.entry;
    if entry.stacked() {
        let ledger = Ledger::for_fork(fork).read()?.entries;
        let tips = opened.bookmark_tips()?;
        let published = tips.get(&BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(entry.publish_remote()),
        });
        let writes = EarlierWrites::read(opened, name, &ledger, published)?;
        return match release::release_reading(opened, entry, (&ledger, &tips), (&writes, created))?
        {
            Ok(reading) => {
                composition_sources(opened, entry, (reading.previous.as_ref(), &reading.members))
            }
            Err(unexplained) => anyhow::bail!("{name}: {unexplained}"),
        };
    }
    release::parent_sources(
        opened,
        entry,
        scheme,
        &opened.parent_commits(created.as_str())?,
    )
}

/// A commit knives did not write, held as every cut is held before it is
/// recorded: the fixed branch's published composition ([`run_release`]) and
/// a release cut outside knives ([`record_hand_cut`]).
struct CommittedCut<'a> {
    commit: &'a CommitId,
    /// Each member with where it came from, as the audit replays them.
    members: &'a [(String, CommitId)],
    /// What the audit measures each member and the drift from
    /// ([`release::audit_cut`]).
    context: release::AuditContext<'a>,
    carried_from: release::CarriedFrom,
}

/// What [`checked_committed_cut`] found, for the cut event to restate.
struct CheckedCut {
    audit: release::CutAudit,
    check: release::CompositionCheck,
}

/// Audit `cut`'s commit ([`report_cut_audit`]), then hold it against the
/// gate's baseline ([`recorded_composition_check`]). `Err` is the exit of a
/// refusal, already reported: nothing is recorded. A file of a fix by hand
/// that the gate holds is the gate's to judge, not the drift check's
/// ([`release::CutAudit::leave_fixes_to_gate`]).
fn checked_committed_cut(
    repo: &RepoName,
    cut: &CommittedCut<'_>,
    (gate, allow_drop): (&CompositionGate<'_>, bool),
) -> anyhow::Result<Result<CheckedCut, Exit>> {
    let mut audit = release::audit_cut(
        gate.path,
        cut.members,
        release::CutSubject::Committed(cut.commit),
        cut.context,
    )?;
    audit.leave_fixes_to_gate(&gate.fixes);
    if let Some(exit) = report_cut_audit(repo, &audit, cut.carried_from, cut.context.drift_from) {
        return Ok(Err(exit));
    }
    let mut subject = release::CutSubject::Committed(cut.commit);
    Ok(
        recorded_composition_check(repo, &mut subject, gate, allow_drop)?
            .map(|check| CheckedCut { audit, check }),
    )
}

/// `knives release cut NAME --at REV`: record the commit `at` names, a
/// release cut outside knives, as `name`'s cut. No commit is made, and no
/// orphan gate, reap, signing or push runs.
///
/// The name and the pulled plan are gated before this ([`requested_cut`],
/// [`release_plan_exit`]), and the commit and its name here
/// ([`hand_cut_gates`]). Its members are its series' reading of it
/// ([`hand_cut_composition`]), audited as every cut is
/// ([`checked_committed_cut`]). The audit replays each member and each
/// parent the trunk holds, and measures the drift from the release the
/// record follows: the previous release a stacked one stacks on, else
/// `follows`, the newest other release of the series, as a cut's audit
/// measures it ([`previous_release_for_record`]). A first record follows no
/// release, and its drift is measured from its members' fork point with the
/// trunk: it is the release every later record's drift is measured from, so
/// a change no member explains is refused in it too. Then the commit is held
/// against the gate's baseline ([`CompositionBaseline`], NAME aside): the
/// members of the series' previous cut and of the newest write of the newest
/// release the ledger records in the series. So a member `include` added
/// after that release's cut is held too, and so is each fix by hand that an
/// `adopt` of any release of the series recorded, while that write still
/// carries it ([`release::adopted_fixes`]); the drift check leaves such a
/// fix's files to the gate. In a stacked series that write, a successor of
/// it or a copy with its tree must be the commit or on its first-parent
/// line ([`release::CompositionDelta::start`]), so a commit built on an
/// older release than that one is refused, however many levels of the
/// stack it leaves out. Each refusal writes nothing. Only then is the local
/// bookmark made, where no ref of the name exists, and the event recorded,
/// which starts the background ledger sweep.
fn record_hand_cut(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    (name, at, allow_drop): (&str, &str, bool),
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
        (&ledger, &writes),
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
        path: &fork.checkout.path,
        parents: &parents,
        base: &composed.base,
        trunk: &trunk,
        tips: &tips,
        start: entry.stacked().then_some(commit),
        fixes,
    };
    // A parent the trunk holds is no member, but the release holds its
    // content: it is replayed, and its files explain drift, as a member's.
    let audited: Vec<(String, CommitId)> = composed
        .members
        .iter()
        .chain(&composed.bases)
        .cloned()
        .collect();
    let committed = CommittedCut {
        commit,
        members: &audited,
        // Nothing is carried from an earlier release: a member this commit
        // lacks is missing, however the commit came to drop it.
        context: release::AuditContext {
            previous: None,
            trunk: &composed.base,
            stacked_on: composed.previous.as_ref().map(|previous| &previous.commit),
            drift_from: Some(composed.drift_from(follows.as_ref().map(|(_, previous)| previous))),
        },
        carried_from: release::CarriedFrom::Unledgered,
    };
    let checked = match checked_committed_cut(repo, &committed, (&gate, allow_drop))? {
        Ok(checked) => checked,
        Err(exit) => return Ok(exit),
    };
    if !held.named {
        knives::jj::set_bookmark(&fork.checkout.path, name, commit.as_str())?;
    }
    // The event records the reading made above: a stacked release's previous
    // release first, under its label, then the members, never a parent the
    // trunk holds.
    let composition: Vec<(String, CommitId)> = composed
        .previous
        .iter()
        .map(PreviousRelease::labelled)
        .chain(composed.members.iter().cloned())
        .collect();
    record_cut_event(
        fork,
        &CompletedCut {
            name,
            request: &release::Cut::from_carried(name.to_owned(), &composed.members),
            created: commit,
            audit: &checked.audit,
            scheme: &entry.release_scheme(),
            held: gate.held(),
            check: &checked.check,
            stated_drop: None,
            // Recorded, not written: knives unioned nothing.
            unions: &[],
            unchecked: "",
            composition: Some(&composition),
        },
        bound,
    )?;
    let publish_remote = entry.publish_remote();
    println!(
        "{}",
        recorded_hand_cut_line(
            repo,
            (name, commit),
            held.published.is_none().then_some(publish_remote)
        )
    );
    // A commit with no conflict leaves no replay inconclusive.
    Ok(Exit::Ok)
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
/// as a cut does: the reap after the next cut keeps only the newest name, and
/// the previous release of a stacked series is the nearest one recorded on
/// its first-parent line.
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

/// What a release cut outside knives composes, as its series reads it.
struct HandCut {
    /// The previous release a release of a stacked series stacks on.
    previous: Option<PreviousRelease>,
    /// Each member with where it came from ([`release::parent_sources`]).
    /// Never a parent the trunk holds.
    members: Vec<(String, CommitId)>,
    /// Each parent of the reading that the trunk holds, with where it came
    /// from: never a member ([`trunk_reaches`]), but content the release
    /// holds all the same.
    bases: Vec<(String, CommitId)>,
    /// What the audit replays each member from and the composition gate's
    /// base: the previous release, or the trunk's fork point with the
    /// members, as for a first cut. With no release to follow, the series'
    /// first record measures its drift from it too ([`Self::drift_from`]).
    base: CommitId,
}

impl HandCut {
    /// What the record's drift is measured from: the previous release it
    /// stacks on, else `follows`, the series' previous release. The series'
    /// first record follows no release, so its drift is measured from
    /// `base`, its members' fork point with the trunk, and a change no member
    /// explains is refused as in a later record.
    fn drift_from<'a>(
        &'a self,
        follows: Option<&'a CommitId>,
    ) -> (&'a CommitId, release::DriftBase) {
        self.previous
            .as_ref()
            .map(|previous| &previous.commit)
            .or(follows)
            .map_or((&self.base, release::DriftBase::ForkPoint), |previous| {
                (previous, release::DriftBase::PreviousRelease)
            })
    }
}

/// The members of `commit`, a release cut outside knives, as its series
/// reads it ([`release::release_reading`]), or the exit after refusing it.
///
/// A parent the trunk holds, which a trunk position reaches, is never a
/// member: it is one of the reading's bases. A stacked series that has a
/// previous release reads `commit` as that release and the side parents on
/// its first-parent line, and refuses it when that line holds a one-parent
/// commit; a start commit on that release is a release with no member.
/// Otherwise its members are its parents, its carriers left out, and it is
/// refused when it merges no member, when a member is `follows`, the
/// series' previous release, which is never a member either, or when a
/// member carries a merge past the trunk: only a stacked series reads a
/// release made on its previous release ([`stacked_record_remedy`]).
fn hand_cut_composition(
    (fork, opened): (&Fork<'_>, &knives::jj::Repo),
    (name, commit): (&str, &CommitId),
    (ledger, writes): (&[knives::ledger::Entry], &EarlierWrites),
    (tips, trunk, follows): (
        &knives::detect::BookmarkTips,
        &CommitId,
        Option<&(BookmarkRef, CommitId)>,
    ),
) -> anyhow::Result<Result<HandCut, Exit>> {
    let (repo, entry) = (&fork.name, fork.entry);
    let (shown, at) = (inline_human_text(name), commit.short());
    let reading = match release::release_reading(opened, entry, (ledger, tips), (writes, commit))? {
        Ok(reading) => reading,
        Err(unexplained) => {
            // The previous release's name is a ledger subject or a ref.
            println!(
                "{repo}: refusing to record {shown} at {at}: {}",
                inline_human_text(&unexplained.to_string())
            );
            return Ok(Err(Exit::Incomplete));
        }
    };
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
    if let Some(previous) = reading.previous {
        return Ok(Ok(HandCut {
            base: previous.commit.clone(),
            previous: Some(previous),
            members,
            bases,
        }));
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
            previous_parent_remedy(fork)
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
            stacked_record_remedy(fork)
        );
        return Ok(Err(Exit::Incomplete));
    }
    let member_tips: Vec<CommitId> = members.iter().map(|(_, tip)| tip.clone()).collect();
    let base = opened.fork_point_of(&member_tips, trunk)?;
    Ok(Ok(HandCut {
        previous: None,
        members,
        bases,
        base,
    }))
}

/// Why a release cut outside knives whose member carries a merge past the
/// trunk is refused, and what records it.
fn stacked_record_remedy(fork: &Fork<'_>) -> String {
    if fork.entry.stacked() {
        return "no previous release of the series is on its first-parent line, so it reads as \
                the series' first cut, which is flat; rebase each member above off the trunk"
            .to_owned();
    }
    format!(
        "a member above stacks on an earlier merge, as a release made as merges on its previous \
         release does; {}, or rebase each member off the trunk",
        stacked_setting(fork)
    )
}

/// Why a release cut outside knives that merges its series' previous
/// release as a member is refused, and what records it.
fn previous_parent_remedy(fork: &Fork<'_>) -> String {
    if fork.entry.stacked() {
        return "a release of a stacked series has its previous release as its first parent, so \
                make it the first one"
            .to_owned();
    }
    format!(
        "such a release is made on its previous release, and {}",
        stacked_setting(fork)
    )
}

/// Which series reads a release made as merges on its previous release,
/// for a series without the `stacked = true` setting: the setting to make.
fn stacked_setting(fork: &Fork<'_>) -> String {
    fork.entry.selected_series.as_ref().map_or_else(
        || {
            "only a named series whose table sets `stacked = true` reads one, and the default \
             series has no table; record it in such a series"
                .to_owned()
        },
        |series| {
            format!(
                "only a series with `stacked = true` on its table reads one; set `stacked = true` \
                 on [repos.{}.series.\"{series}\"] to record it",
                fork.name
            )
        },
    )
}

fn report_completed_cut(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    cut: &CompletedCut<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let entry = fork.entry;
    let mut post_cut_exit =
        if cut.audit.inconclusive.is_empty() && cut.check.inconclusive.is_empty() {
            Exit::Ok
        } else {
            Exit::Findings
        };
    print_previous_release_position(opened, entry);
    println!(
        "  cut {} as {} with {} parent(s), not pushed",
        cut.name,
        cut.created.short(),
        cut.request.parents.len()
    );
    match knives::jj::conflicted_files(&fork.checkout.path, cut.created.as_str()) {
        Ok(files) => println!(
            "{}",
            release::conflict_guidance(cut.name, &files, cut.unions)
        ),
        Err(error) => println!("  could not list conflicts: {error}"),
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
    // read names no workspace rather than stopping the reap.
    let claims = knives::store::Store::open(
        knives::store::default_state_path(),
        &bound.registry.former_names(),
        &[],
    );
    match claims {
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
    let exit = post_cut_exit.worst(reap_after_cut(fork)?);
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

fn report_orphaned_cut(
    repo: &RepoName,
    orphaned: &OrphanedLineage,
    allow_drop: bool,
) -> Option<Exit> {
    // The previous release is a ref read from the repository.
    let previous = inline_human_text(&orphaned.previous);
    if allow_drop {
        println!(
            "{repo}: --allow-drop: cutting without {} commit(s) reachable only from {previous} \
             or its descendants; what is dropped is said once the cut is done",
            orphaned.commits.len()
        );
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
    Some(Exit::Incomplete)
}

/// The commits the cut would strand, reachable only from the previous
/// release or its descendants, or `None`.
///
/// A stacked series strands none: its cut keeps the previous release as its
/// first parent, and the reap after it abandons nothing the cut descends
/// from, so that release and every descendant of it stay visible.
fn check_orphan_commits_before_cut(
    opened: &knives::jj::Repo,
    fork: &Fork<'_>,
) -> anyhow::Result<Option<OrphanedLineage>> {
    let entry = fork.entry;
    if entry.stacked() {
        return Ok(None);
    }
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

/// Reap superseded dated cuts now that a newer one exists.
///
/// Under `Fixed` the enumeration is empty by construction (no dated names), so
/// this is a no-op there. Opens the repository again deliberately: the caller's
/// handle predates the cut and reads stale tips, under which the superseded cut
/// is still the newest dated name and nothing is reaped at all.
fn reap_after_cut(fork: &Fork<'_>) -> anyhow::Result<Exit> {
    let reaped = reap(fork)?;
    print_reap(&fork.name, &reaped);
    Ok(reap_exit(&reaped.report))
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

/// Reap superseded dated cuts on demand, in the chosen output.
pub(crate) fn run_reap(fork: &Fork<'_>, output: knives::cli::Output) -> anyhow::Result<Exit> {
    let reaped = reap(fork)?;
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
/// after it.
///
/// A stacked series first forgets each superseded name the live release
/// descends from ([`release::forget_superseded_ancestors`]): it is reported
/// with the names whose commit stays, and nothing of it is abandoned, its
/// earlier writes included. Every other name keeps the rules below.
fn reap(fork: &Fork<'_>) -> anyhow::Result<Reaped> {
    let path = &fork.checkout.path;
    let (scheme, publish_remote) = (fork.entry.dated_scheme(), fork.entry.publish_remote());
    let descended = if fork.entry.stacked() {
        release::forget_superseded_ancestors(
            &knives::jj::Repo::open(path)?,
            &scheme,
            publish_remote,
        )?
    } else {
        Vec::new()
    };
    let opened = knives::jj::Repo::open(path)?;
    let ledger = Ledger::for_fork(fork).read()?.entries;
    let mut report = release::reap_superseded(&opened, &scheme, publish_remote, &ledger)?;
    let cleaned = abandon_reaped_writes(fork, &report, &ledger);
    report.forgotten_only.extend(descended);
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
    /// Names whose refs were forgotten while a pin kept the commit, or, in a
    /// stacked series, while the live release descends from it.
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
/// on it — someone's stacked work, or a workspace's working copy — or could not
/// finish. A commit kept because a tag or another bookmark still pins it is
/// neither: every ref knives owns is gone and the commit stays on purpose, so
/// there is nothing to act on, and a cut whose reap met one must not exit
/// non-zero for as long as the pin exists. Neither is a cut kept only for
/// what its own name published on it: consumers were given that, and
/// the newer cut redoes it.
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
    use knives::commands::release::{CompositionCheck, ReapReport, RecordedMember};
    use knives::ids::CommitId;

    /// ESC opens a C0 sequence, U+009B is the C1 CSI; neither may reach a
    /// terminal.
    const HOSTILE: &str = "x\u{1b}]0;pwned\u{7}\u{9b}2J";
    const ESCAPED: &str = "x\u{fffd}]0;pwned\u{fffd}\u{fffd}2J";

    fn assert_escaped(lines: &[String]) {
        for line in lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
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
            off_line: true,
            dropped_fixes: Vec::new(),
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
                fixes: &[],
            },
            HeldRecords {
                first: HeldRecord::NewestWrite(&write),
                other: Some(HeldRecord::PreviousCut(&cut)),
                record: Some(("release/2026-11-14", &at)),
                fixes: &[],
            },
        ];
        for held in gates {
            for on_drop in [
                DropDisposition::Allowed,
                DropDisposition::RefuseDiscardingCandidate,
            ] {
                lines.extend(uncarried_cut_lines(&repo, held, &check, on_drop));
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
        for expected in [
            format!("the previous cut release/{ESCAPED} recorded"),
            format!("which release/{ESCAPED}.2's newest recorded write (an edit) holds"),
            format!("of release/{ESCAPED}.2, the newest release that the ledger records"),
            format!("    {ESCAPED} (not known to this repository)"),
            format!("    feat/{ESCAPED}@abab"),
            format!("feat/{ESCAPED}@aaaa: recorded by the previous cut release/{ESCAPED}"),
            format!("feat/{ESCAPED}@bbbb is carried by cccc"),
            format!(
                "release/{ESCAPED}.2's newest recorded write (an edit) {} on its first-parent \
                 line",
                ids_prefix(ESCAPED)
            ),
            format!("`jj bookmark set 'release/{ESCAPED}.2' -r '"),
        ] {
            assert!(text.contains(&expected), "{expected:?} not in {text:?}");
        }
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
            fixes: &[],
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
    fn the_fix_of_an_adopt_from_the_ledger_reaches_the_terminal_escaped() {
        // Given: an adopt's release and commit, which are a ledger entry's
        // subject and first evidence string, and a file of its fix, a path
        // read from the repository.
        let (name, commit) = (format!("release/{HOSTILE}"), CommitId::new(HOSTILE));

        // When: a message names the fix, and the cut event names it raw.
        let shown = format!(
            "{}{}",
            fix_title((&name, &commit), inline_human_text),
            fix_files(&[format!("fix{HOSTILE}.txt")])
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
        // And: with no file, the message says nothing here measured the fix.
        assert_eq!(
            fix_files(&[]),
            " (nothing here can measure it: this repository does not hold the adopt or cannot \
             read its release)"
        );
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
