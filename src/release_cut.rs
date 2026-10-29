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
use knives::ids::{BookmarkRef, ReleaseScheme, RemoteName, RepoName};
use knives::ledger::{Draft, Kind, Ledger};
use knives::release_model::{
    RecordedCut, StackedHistoryContext, carried_branches, last_series_cut, members_event_text,
    previous_release_for_cut, series_members, trunk_positions,
};

use super::release_edit::recorded_parents;
use super::scribe_for;

pub(crate) enum ReleaseInvocation {
    Plan,
    Cut {
        name: Option<String>,
        allow_drop: bool,
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
    let (cut_name, allow_drop) = match requested_cut(invocation, &scheme, entry) {
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
        // A dated cut takes a name that sorts after the newest release: the reap
        // that follows every dated cut keeps only the newest name, so any other
        // name would be created and taken straight back as superseded in the
        // same command, reported as a success that left nothing behind.
        if let Some((previous_ref, _)) = &previous
            && matches!(scheme, ReleaseScheme::Dated | ReleaseScheme::Series(_))
            && scheme.cut_order(&name) <= scheme.cut_order(previous_ref.branch().as_str())
        {
            println!(
                "{repo}: refusing to cut {name}: {} is the newest release, so a dated cut takes a name that sorts after it (the reap after a cut keeps only the newest name)",
                previous_ref.branch()
            );
            return Ok(Exit::Incomplete);
        }
        // A cut is a new name for the composition in hand, never a recomputation:
        // with a previous release its parents are carried verbatim — nothing joins,
        // nothing advances, and a branch enters through `release include`. Only the
        // first cut has no composition to carry, so it starts from every branch: a
        // release is a flat merge of feature and fix branches, and the upstream
        // base is never a direct parent — it is reachable through every member.
        let (carried, members, audit_base) = if let Some((_, previous)) = &previous {
            let parents = opened.parent_commits(previous.as_str())?;
            let carried = release::parent_sources(&opened, entry, &scheme, &parents)?;
            let members = carried.clone();
            let base =
                release::shared_base(&opened, previous, &trunk)?.unwrap_or_else(|| trunk.clone());
            (carried, members, base)
        } else {
            // The first cut of a series composes only that series' branches:
            // a branch on another series' base would carry that base in.
            let (carried, unfit) = series_members(
                &opened,
                entry,
                carried_branches(&opened, entry.trunk(), &scheme)?,
            )?;
            if !unfit.is_empty() {
                for line in &unfit {
                    println!("{repo}: {line}");
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
            let base = opened
                .common_ancestor(&member_tips, &trunk)?
                .unwrap_or_else(|| trunk.clone());
            (carried, members, base)
        };
        let request = release::Cut::from_carried(name.clone(), &carried);
        let mut candidate = release::candidate_cut(path, &request, previous_commit.as_ref())?;
        // The orphan gate protects unreachable commits; this protects the
        // composition itself. The previous cut's ledger event is the only
        // record of a parent set that survives the bookmark moving, so every
        // subject is held against it before anything is recorded or published.
        let gate = CompositionGate {
            ledger: Ledger::for_fork(fork),
            entry,
            opened: &opened,
            path,
            parents: &request.parents,
            base: &audit_base,
            trunk: &trunk,
            tips: &tips,
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
                let frozen_previous =
                    matches!(scheme, ReleaseScheme::Dated | ReleaseScheme::Series(_))
                        && release::repair_effect(&pins, previous_ref.branch(), Some(published))
                            == release::RepairEffect::NewDatedName;
                let published_previous = release::published_release(
                    &opened,
                    &gate.ledger.read()?.entries,
                    release::ReleaseAt {
                        scheme: &scheme,
                        name: previous_ref.branch().as_str(),
                        commits: &[Some(published), Some(previous_tip)],
                    },
                )?;
                if frozen_previous {
                    println!(
                        "{repo}: {name} starts identical to {}@{publish_remote} ({}); every pin of {} is frozen, so this new name is the composition to edit — `include`, `advance`, `drop` or `rebase` it before pushing",
                        previous_ref.branch(),
                        published.short(),
                        previous_ref.branch()
                    );
                } else if let Some(evidence) = published_previous {
                    println!(
                        "{repo}: {name} starts identical to {}@{publish_remote} ({}); {} is published ({evidence}), so this new name is the composition to edit — `include`, `advance`, `drop` or `rebase` it before pushing",
                        previous_ref.branch(),
                        published.short(),
                        previous_ref.branch()
                    );
                } else if matches!(scheme, ReleaseScheme::Fixed(_))
                    // The pulled plan refused on any problem this read finds.
                    && let previous_recorded =
                        last_series_cut(&gate.ledger.read()?.entries, entry)
                    && previous_recorded
                        .as_ref()
                        .is_none_or(|recorded| recorded.commit != *published)
                {
                    let audit = release::audit_cut(
                        path,
                        &members,
                        release::CutSubject::Committed(published),
                        release::AuditContext {
                            previous: previous_commit.as_ref(),
                            trunk: &audit_base,
                        },
                    )?;
                    if let Some(exit) = report_cut_audit(repo, &audit) {
                        return Ok(exit);
                    }
                    let mut subject = release::CutSubject::Committed(published);
                    let (recorded, check) =
                        match recorded_composition_check(repo, &mut subject, &gate, allow_drop)? {
                            Ok(verdict) => verdict,
                            Err(exit) => return Ok(exit),
                        };
                    let completed = CompletedCut {
                        name: &name,
                        request: &request,
                        created: published,
                        audit: &audit,
                        scheme: &scheme,
                        recorded: recorded.as_ref(),
                        check: &check,
                        stated_drop: None,
                    };
                    record_cut_event(fork, &completed, bound)?;
                    match &previous_recorded {
                        Some(recorded) => println!(
                            "{repo}: recorded published composition {name}@{publish_remote} ({}) as the cut; previous recorded cut {} was {}",
                            published.short(),
                            recorded.name,
                            recorded.commit.short()
                        ),
                        None => println!(
                            "{repo}: recorded published composition {name}@{publish_remote} ({}) as the cut; no previous recorded cut",
                            published.short()
                        ),
                    }
                    if let Some(orphaned) = &orphaned {
                        report_stated_drop(fork, orphaned)?;
                    }
                    if !audit.inconclusive.is_empty() || !check.inconclusive.is_empty() {
                        worst = worst.worst(Exit::Findings);
                    }
                    return Ok(worst);
                } else {
                    println!(
                        "{repo}: refusing to cut {name}: identical to {}@{publish_remote} ({}); nothing to cut",
                        previous_ref.branch(),
                        published.short()
                    );
                    return Ok(Exit::Incomplete);
                }
            }
        }
        // An audit error or failure simply DROPS the candidate: the merge
        // was never a published operation, so there is nothing to abandon
        // and no crash window that strands one.
        let audit = release::audit_cut(
            path,
            &members,
            release::CutSubject::Candidate(&mut candidate),
            release::AuditContext {
                previous: previous_commit.as_ref(),
                trunk: &audit_base,
            },
        )?;
        if let Some(exit) = report_cut_audit(repo, &audit) {
            return Ok(exit);
        }
        let (recorded, check) = {
            let mut subject = release::CutSubject::Candidate(&mut candidate);
            match recorded_composition_check(repo, &mut subject, &gate, allow_drop)? {
                Ok(verdict) => verdict,
                Err(exit) => return Ok(exit),
            }
        };
        let created = release::publish_cut(candidate, &request.name, &scheme)?;
        let completed = CompletedCut {
            name: &name,
            request: &request,
            created: &created,
            audit: &audit,
            scheme: &scheme,
            recorded: recorded.as_ref(),
            check: &check,
            stated_drop: orphaned.as_ref(),
        };
        record_cut_event(fork, &completed, bound)?;
        worst = worst.worst(report_completed_cut(fork, &opened, &completed, bound)?);
    }
    Ok(worst)
}

fn requested_cut(
    invocation: &ReleaseInvocation,
    scheme: &knives::ids::ReleaseScheme,
    entry: &knives::config::RepoEntry,
) -> Result<(Option<String>, bool), Exit> {
    match invocation {
        ReleaseInvocation::Plan => Ok((None, false)),
        ReleaseInvocation::Cut { name, allow_drop } => {
            match release::cut_name(scheme, name.as_deref())
                .and_then(|name| release::clear_of_series_prefixes(entry, name))
            {
                Ok(name) => Ok((Some(name), *allow_drop)),
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
        println!("  !! {lag}");
        exit = exit.worst(Exit::Findings);
    }
    Ok((exit, plan.pins))
}

/// Say what the audit found; refuse when it failed.
///
/// Inconclusive members are reported without failing the cut: a conflicted
/// replay onto a conflicted cut answers nothing either way. Missing or
/// unexplained content refuses the cut, because naming it would look exactly
/// like success while work is gone. The refused candidate was never a
/// published operation, so nothing is abandoned and nothing needs cleanup.
fn report_cut_audit(repo: &RepoName, audit: &release::CutAudit) -> Option<Exit> {
    for name in &audit.carried {
        println!(
            "  {name}: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        );
    }
    for name in &audit.inconclusive {
        println!(
            "  {name}: content check inconclusive (replay conflicted; \
             re-check after resolving the cut's conflicts)"
        );
    }
    if audit.passed() {
        return None;
    }
    for name in &audit.missing {
        println!("  !! {name}: the cut tree is missing or diverges from the member's content");
    }
    for file in &audit.unexplained {
        println!(
            "  !! {file}: changed between the previous release and this cut \
             with no member or trunk explaining it"
        );
    }
    println!("{repo}: cut discarded; nothing was written at all. Fix the inputs and re-cut.");
    Some(Exit::Incomplete)
}

/// The composition gate's inputs: everything the recorded-member check reads
/// beside the cut subject.
struct CompositionGate<'a> {
    /// The fork's ledger, where the previous cut's composition is recorded.
    ledger: Ledger,
    /// The entry as the cut's series selects it: only that series' cuts are
    /// its previous composition.
    entry: &'a knives::config::RepoEntry,
    opened: &'a knives::jj::Repo,
    path: &'a std::path::Path,
    parents: &'a [knives::ids::CommitId],
    base: &'a knives::ids::CommitId,
    trunk: &'a knives::ids::CommitId,
    tips: &'a knives::detect::BookmarkTips,
}

/// Hold the cut subject against the previous cut's recorded composition.
///
/// `Err` is the refusal, already reported. `Ok` carries what the ledger
/// recorded and what the check found, for the cut event to restate.
fn recorded_composition_check(
    repo: &RepoName,
    subject: &mut release::CutSubject<'_>,
    gate: &CompositionGate<'_>,
    allow_drop: bool,
) -> anyhow::Result<Result<(Option<RecordedCut>, release::CompositionCheck), Exit>> {
    // The pulled plan refused on any problem this read finds.
    let recorded = last_series_cut(&gate.ledger.read()?.entries, gate.entry);
    let check = match &recorded {
        Some(recorded) => release::uncarried_recorded_members(
            gate.opened,
            gate.path,
            subject,
            &release::CompositionDelta {
                recorded,
                parents: gate.parents,
                base: gate.base,
                trunk: gate.trunk,
                tips: gate.tips,
            },
        )?,
        None => release::CompositionCheck::default(),
    };
    let on_drop = match (allow_drop, &*subject) {
        (true, _) => DropDisposition::Allowed,
        (false, release::CutSubject::Candidate(_)) => DropDisposition::RefuseDiscardingCandidate,
        (false, release::CutSubject::Committed(_)) => DropDisposition::RefuseRecordingNothing,
    };
    if let Some(exit) = report_uncarried_cut(repo, recorded.as_ref(), &check, on_drop) {
        return Ok(Err(exit));
    }
    Ok(Ok((recorded, check)))
}

/// What a cut does about members the previous cut recorded but this one lacks.
#[derive(Clone, Copy)]
enum DropDisposition {
    /// `--allow-drop`: state the drop and carry on.
    Allowed,
    /// Refuse; the candidate commit was never published, so it is simply gone.
    RefuseDiscardingCandidate,
    /// Refuse; the composition is already on origin, so nothing changes there.
    RefuseRecordingNothing,
}

/// Say what the recorded-composition check found; refuse when members are gone.
///
/// Inconclusive members are reported without refusing, for the same reason the
/// audit's are: a conflicted replay onto a conflicted cut subject answers
/// nothing either way. A member the cut subject does not carry refuses the cut,
/// the previous cut's ledger event is the only surviving record of the
/// composition — every edit moves the bookmark and abandons the commit it
/// replaced. A refused candidate was never published and a refused
/// record leaves origin as it was, so nothing is abandoned and nothing needs
/// cleanup.
fn report_uncarried_cut(
    repo: &RepoName,
    recorded: Option<&RecordedCut>,
    check: &release::CompositionCheck,
    on_drop: DropDisposition,
) -> Option<Exit> {
    let recorded = recorded?;
    for (member, twin) in &check.twins {
        println!(
            "{repo}: {member} is carried by {}, which has the same tree",
            twin.short()
        );
    }
    for member in &check.inconclusive {
        println!(
            "  {}: recorded by the {} cut; carry check inconclusive (replay conflicted; \
             re-check after resolving the cut's conflicts)",
            member.name, recorded.name
        );
    }
    if check.dropped.is_empty() {
        return None;
    }
    if matches!(on_drop, DropDisposition::Allowed) {
        println!(
            "{repo}: --allow-drop: cutting without {} member(s) the previous cut {} recorded: {}",
            check.dropped.len(),
            recorded.name,
            check.dropped.join(", ")
        );
        return None;
    }
    println!(
        "{repo}: refusing to cut: the previous cut {} recorded {} member(s) this cut does not \
         carry:",
        recorded.name,
        check.dropped.len()
    );
    for member in &check.dropped {
        println!("    {member}");
    }
    let outcome = match on_drop {
        DropDisposition::RefuseDiscardingCandidate => "the candidate was discarded",
        DropDisposition::Allowed | DropDisposition::RefuseRecordingNothing => {
            "nothing was recorded"
        }
    };
    println!(
        "  {outcome}; `knives release include <branch>` restores a member, \
         or re-run with --allow-drop to state the drop is intended"
    );
    Some(Exit::Incomplete)
}

struct CompletedCut<'a> {
    name: &'a str,
    request: &'a release::Cut,
    created: &'a knives::ids::CommitId,
    audit: &'a release::CutAudit,
    scheme: &'a ReleaseScheme,
    recorded: Option<&'a RecordedCut>,
    check: &'a release::CompositionCheck,
    /// What `--allow-drop` let the cut go ahead without, said after the reap.
    stated_drop: Option<&'a OrphanedLineage>,
}

/// Record which branches and commits became a published release cut.
///
/// The text names the cut's change id beside its commit id. Resolving the
/// cut's conflicts before pushing rewrites the merge, so the commit the event
/// names is not the commit the release remote ends up holding, while the
/// change id survives the
/// rewrite and still resolves to the release. Evidence keeps the commit first:
/// the composition gate parses it from that position.
fn record_cut_event(
    fork: &Fork<'_>,
    cut: &CompletedCut<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    let entry = fork.entry;
    let opened = knives::jj::Repo::open(&fork.checkout.path)?;
    let parents = opened.parent_commits(cut.created.as_str())?;
    let change = opened.change_id_of(cut.created.as_str())?;
    let members = release::parent_sources(&opened, entry, cut.scheme, &parents)?;
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
    let delta = cut.recorded.map_or_else(String::new, |recorded| {
        let carried = if cut.check.dropped.is_empty() {
            "all carried".to_owned()
        } else {
            format!("dropped: {}", cut.check.dropped.join(", "))
        };
        let unverified = if cut.check.inconclusive.is_empty() {
            String::new()
        } else {
            format!(
                "; unverified: {}",
                cut.check
                    .inconclusive
                    .iter()
                    .map(|member| member.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "; previous cut {} recorded {} member(s); {carried}{unverified}",
            recorded.name,
            recorded.members.len()
        )
    });
    scribe_for(fork, bound)?.record(&Draft {
        subject: Some(cut.name),
        kind: Kind::Event,
        disposition: None,
        statement: None,
        text: format!(
            "cut {} as {} (change {}) with {} parent(s): {members_text}{delta}",
            cut.name,
            cut.created.short(),
            change.short(),
            members.len()
        ),
        evidence,
        pr: None,
        parents: recorded_parents(&opened, entry, &parents)?,
    })?;
    Ok(())
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
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
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
            println!("  could not read claims, so no workspace is named for removal: {error}");
        }
        Ok(store) => {
            live.extend(
                store
                    .claims(Some(&fork.upstream))
                    .into_iter()
                    .map(|claim| claim.branch.clone()),
            );
            let orphans = release::workspaces_to_clean(&present, &recorded, &live);
            if orphans.is_empty() {
                println!("  no workspace belongs to a branch that no longer exists");
            } else {
                println!(
                    "  {} workspace(s) belong to branches that no longer exist: {}",
                    orphans.len(),
                    orphans.join(", ")
                );
                println!(
                    "  no bookmark or live claim holds them; remove with `jj workspace forget \
                     <name>` once you have checked them"
                );
            }
        }
    }
    let exit = post_cut_exit.worst(reap_after_cut(fork)?);
    if let Some(orphaned) = cut.stated_drop {
        report_stated_drop(fork, orphaned)?;
    }
    Ok(exit)
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
    if allow_drop {
        println!(
            "{repo}: --allow-drop: cutting without {} commit(s) reachable only from {} or its \
             descendants; what is dropped is said once the cut is done",
            orphaned.commits.len(),
            orphaned.previous
        );
        return None;
    }
    println!(
        "{repo}: refusing to cut: {} commit(s) are reachable only from \
         {} or its descendants and would be dropped:",
        orphaned.commits.len(),
        orphaned.previous
    );
    for commit in &orphaned.commits {
        println!("    {}", commit.short());
    }
    println!("  re-run with --allow-drop to state this is intended");
    Some(Exit::Incomplete)
}

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
    let mut refused = false;
    for (branch, tip) in carried {
        if let Some(stacked) = knives::release_model::stacked_history(context, branch, tip)? {
            println!(
                "{repo}: {}; rebase it off the trunk before cutting",
                stacked.detail
            );
            refused = true;
        }
    }
    Ok(refused)
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
/// for good. Returns each abandoned commit with its name, and a note for each
/// name whose cleanup failed: the reap itself is already committed, so its
/// report must still reach the operator.
fn abandon_reaped_writes(
    fork: &Fork<'_>,
    report: &knives::commands::release::ReapReport,
    ledger: &[knives::ledger::Entry],
) -> (Vec<(String, knives::ids::CommitId)>, Vec<String>) {
    let names: Vec<&str> = report
        .reaped
        .iter()
        .chain(report.forgotten_only.iter().map(|(name, _)| name))
        .map(String::as_str)
        .collect();
    let mut abandoned_earlier = Vec::new();
    let mut notes = Vec::new();
    if names.is_empty() {
        return (abandoned_earlier, notes);
    }
    let reopened = match knives::jj::Repo::open(&fork.checkout.path) {
        Ok(reopened) => reopened,
        Err(error) => {
            notes.push(format!("earlier writes not cleaned up: {error}"));
            return (abandoned_earlier, notes);
        }
    };
    for name in names {
        let abandoned = release::earlier_writes(&reopened, ledger, name).and_then(|earlier| {
            knives::jj::abandon_unheld_writes(
                &fork.checkout.path,
                &earlier,
                &format!("knives: {name}: abandon earlier writes nothing rests on"),
            )
        });
        match abandoned {
            Ok(abandoned) => abandoned_earlier.extend(
                abandoned
                    .into_iter()
                    .map(|commit| (name.to_owned(), commit)),
            ),
            Err(error) => notes.push(format!("{name}: earlier writes not cleaned up: {error}")),
        }
    }
    (abandoned_earlier, notes)
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

/// What one reap did: the report, and the earlier writes it abandoned after.
struct Reaped {
    report: knives::commands::release::ReapReport,
    abandoned_earlier: Vec<(String, knives::ids::CommitId)>,
}

/// The reap both callers run, on the repository as it is now. The ledger's
/// earlier writes of each name count for the keep rule and are cleaned up
/// after it.
fn reap(fork: &Fork<'_>) -> anyhow::Result<Reaped> {
    let path = &fork.checkout.path;
    let opened = knives::jj::Repo::open(path)?;
    let ledger = Ledger::for_fork(fork).read()?.entries;
    let report = release::reap_superseded(
        &opened,
        &fork.entry.dated_scheme(),
        fork.entry.publish_remote(),
        &ledger,
    )?;
    let (abandoned_earlier, notes) = abandon_reaped_writes(fork, &report, &ledger);
    let mut report = report;
    report.notes.extend(notes);
    Ok(Reaped {
        report,
        abandoned_earlier,
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
        }
    }
}

/// Return a finding when reaping leaves a cut behind because something rests
/// on it — someone's stacked work, or a workspace's working copy — or could not
/// finish. A commit kept because a tag or another bookmark still pins it is
/// neither: every ref knives owns is gone and the commit stays on purpose, so
/// there is nothing to act on, and a cut whose reap met one must not exit
/// non-zero for as long as the pin exists.
const fn reap_exit(report: &knives::commands::release::ReapReport) -> Exit {
    if report.kept.is_empty() && report.notes.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

fn print_reap(repo: &RepoName, reaped: &Reaped) {
    let report = &reaped.report;
    if report.reaped.is_empty() && report.forgotten_only.is_empty() && report.kept.is_empty() {
        println!("{repo}: nothing to reap");
    }
    for name in &report.reaped {
        println!(
            "{repo}: reaped {name} (refs forgotten everywhere, commit abandoned; remote untouched)"
        );
    }
    for (name, why) in &report.forgotten_only {
        println!("{repo}: reaped {name} (refs forgotten everywhere; commit kept, {why})");
    }
    for (name, reason) in &report.kept {
        println!("{repo}: kept {name}: {reason}");
    }
    for note in &report.notes {
        println!("{repo}: ! {note}");
    }
    for (name, commit) in &reaped.abandoned_earlier {
        super::release_edit::report_abandoned_earlier(repo, name, std::slice::from_ref(commit));
    }
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
