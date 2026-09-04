//! `knives release` (plan and cut) and `knives release reap`.
//!
//! A cut is one merge of the carried members, gated on what the previous cut
//! recorded, what the working copies hold, and what would be orphaned; the
//! ledger event and the superseded-cut reap follow it. Rebasing the
//! composition lives in `release_rebase`; the membership verbs in
//! `release_edit`.

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::release;
use knives::forge::github::CliForge;
use knives::ids::{ReleaseScheme, RepoName};
use knives::ledger::{Draft, Kind, Ledger};
use knives::release_model::{
    RecordedCut, StackedHistoryContext, carried_branches, last_recorded_cut, members_event_text,
    previous_release_for_cut, trunk_positions,
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
    bound: Option<&RepoName>,
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
    let (cut_name, allow_drop) = match requested_cut(invocation, &scheme) {
        Ok(request) => request,
        Err(exit) => return Ok(exit),
    };
    let mut worst = release_plan_exit(
        fork,
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
        let trunk_name = entry.upstream_trunk();
        let trunk = opened.resolve_commit(&trunk_name)?;
        if let Some(orphaned) = check_orphan_commits_before_cut(&opened, fork)?
            && let Some(exit) = report_orphaned_cut(repo, &orphaned, allow_drop)
        {
            return Ok(exit);
        }
        let tips = opened.bookmark_tips()?;
        let previous = previous_release_for_cut(entry, &tips);
        let previous_commit = previous.as_ref().map(|(_, commit)| commit.clone());
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
            let carried = carried_branches(&opened, entry.trunk(), &scheme)?;
            if carried.is_empty() {
                println!(
                    "{repo}: no branches to cut; a release is a flat merge of feature \
                     and fix branches, and there are none"
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
        // The orphan gate protects unreachable commits; this protects the
        // composition itself. The previous cut's ledger event is the only
        // record of a parent set that survives the bookmark moving, so the
        // candidate is held against it before anything is published.
        let gate = CompositionGate {
            opened: &opened,
            parents: &request.parents,
            base: &audit_base,
            trunk: &trunk,
            tips: &tips,
        };
        let (recorded, check) =
            match recorded_composition_check(repo, &mut candidate, &gate, allow_drop)? {
                Ok(verdict) => verdict,
                Err(exit) => return Ok(exit),
            };
        let created = release::publish_cut(candidate, &request.name, &scheme)?;
        let completed = CompletedCut {
            name: &name,
            request: &request,
            carried: &carried,
            created: &created,
            audit: &audit,
            scheme: &scheme,
            recorded: recorded.as_ref(),
            check: &check,
        };
        record_cut_event(fork, &completed, bound)?;
        worst = worst.worst(report_completed_cut(fork, &opened, &completed)?);
    }
    Ok(worst)
}

fn requested_cut(
    invocation: &ReleaseInvocation,
    scheme: &knives::ids::ReleaseScheme,
) -> Result<(Option<String>, bool), Exit> {
    match invocation {
        ReleaseInvocation::Plan => Ok((None, false)),
        ReleaseInvocation::Cut { name, allow_drop } => {
            match release::cut_name(scheme, name.as_deref()) {
                Ok(name) => Ok((Some(name), *allow_drop)),
                Err(message) => {
                    eprintln!("{message}");
                    Err(Exit::Usage)
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the release plan needs explicit repository state and each independently owned consumer-scan collaborator"
)]
fn release_plan_exit(
    fork: &Fork<'_>,
    locals: &[std::path::PathBuf],
    opened: &knives::jj::Repo,
    forge: &dyn knives::consumer_pins::ConsumerPinSource,
    cache_root: Option<&std::path::Path>,
    heads: &knives::consumer_pins::ConsumerHeadMemo,
) -> anyhow::Result<Exit> {
    let entry = fork.entry;
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals,
        forge,
        cache_root,
        heads,
    };
    let plan = release::plan(fork, &consumers, &Ledger::for_repo(&fork.name).entries()?)?;
    println!("{}", release::render(&plan));
    let mut exit = release::exit_for(&plan);
    if let Some(lag) = release::trunk_lag(opened, plan.release.as_deref(), &entry.upstream_trunk())
    {
        println!("  !! {lag}");
        exit = exit.worst(Exit::Findings);
    }
    Ok(exit)
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
/// beside the candidate itself.
struct CompositionGate<'a> {
    opened: &'a knives::jj::Repo,
    parents: &'a [knives::ids::CommitId],
    base: &'a knives::ids::CommitId,
    trunk: &'a knives::ids::CommitId,
    tips: &'a knives::detect::BookmarkTips,
}

/// Hold the candidate against the previous cut's recorded composition.
///
/// `Err` is the refusal, already reported. `Ok` carries what the ledger
/// recorded and what the check found, for the cut event to restate.
fn recorded_composition_check(
    repo: &RepoName,
    candidate: &mut knives::jj::Candidate,
    gate: &CompositionGate<'_>,
    allow_drop: bool,
) -> anyhow::Result<Result<(Option<RecordedCut>, release::CompositionCheck), Exit>> {
    let recorded = last_recorded_cut(&Ledger::for_repo(repo).entries()?, None);
    let check = match &recorded {
        Some(recorded) => release::uncarried_recorded_members(
            gate.opened,
            candidate,
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
    if let Some(exit) = report_uncarried_cut(repo, recorded.as_ref(), &check, allow_drop) {
        return Ok(Err(exit));
    }
    Ok(Ok((recorded, check)))
}

/// Say what the recorded-composition check found; refuse when members are gone.
///
/// Inconclusive members are reported without refusing, for the same reason the
/// audit's are: a conflicted replay onto a conflicted candidate answers nothing
/// either way. A member the candidate does not carry refuses the cut, because
/// the previous cut's ledger event is the only surviving record of the
/// composition — every edit moves the bookmark, and the next cut reaps the
/// superseded commit. The refused candidate was never published, so nothing is
/// abandoned and nothing needs cleanup.
fn report_uncarried_cut(
    repo: &RepoName,
    recorded: Option<&RecordedCut>,
    check: &release::CompositionCheck,
    allow_drop: bool,
) -> Option<Exit> {
    let recorded = recorded?;
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
    if allow_drop {
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
    println!(
        "  the candidate was discarded; `knives release include <branch>` restores a member, \
         or re-run with --allow-drop to state the drop is intended"
    );
    Some(Exit::Incomplete)
}

struct CompletedCut<'a> {
    name: &'a str,
    request: &'a release::Cut,
    carried: &'a [(String, knives::ids::CommitId)],
    created: &'a knives::ids::CommitId,
    audit: &'a release::CutAudit,
    scheme: &'a ReleaseScheme,
    recorded: Option<&'a RecordedCut>,
    check: &'a release::CompositionCheck,
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
    bound: Option<&RepoName>,
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
    // A branch that still exists locally is not dropped, merely not carried;
    // only a workspace whose branch is gone entirely is left-behind cruft.
    let mut carried_names: Vec<String> = cut
        .carried
        .iter()
        .map(|(branch, _)| branch.clone())
        .collect();
    carried_names.extend(
        carried_branches(opened, entry.trunk(), &entry.release_scheme())?
            .into_iter()
            .map(|(branch, _)| branch),
    );
    let orphans = release::workspaces_to_clean(&present, &carried_names);
    if orphans.is_empty() {
        println!("  every workspace still has a branch");
    } else {
        println!(
            "  {} workspace(s) belong to branches that no longer exist: {}",
            orphans.len(),
            orphans.join(", ")
        );
        println!("  remove with `jj workspace forget <name>` once you have checked them");
    }
    Ok(post_cut_exit.worst(reap_after_cut(fork)?))
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
            "{repo}: --allow-drop: dropping {} commit(s) from the old lineage",
            orphaned.commits.len()
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
        previous: previous.0,
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
    let path = &fork.checkout.path;
    let reopened = knives::jj::Repo::open(path)?;
    let report = release::reap_superseded(path, &reopened, fork.entry.publish_remote())?;
    print_reap(fork.name.as_str(), &report);
    Ok(reap_exit(&report))
}

/// Reap superseded dated cuts on demand.
pub(crate) fn run_reap(fork: &Fork<'_>) -> anyhow::Result<Exit> {
    let path = &fork.checkout.path;
    let opened = knives::jj::Repo::open(path)?;
    let report = release::reap_superseded(path, &opened, fork.entry.publish_remote())?;
    print_reap(fork.name.as_str(), &report);
    Ok(reap_exit(&report))
}

/// Return a finding when reaping leaves work behind — a cut with descendants
/// is someone's stacked work — or could not finish. A commit kept because a
/// tag or someone else's bookmark still pins it is neither: every ref knives
/// owns is gone and the commit stays on purpose, so there is nothing to act on,
/// and a cut whose reap met one must not exit non-zero for as long as the tag
/// exists.
const fn reap_exit(report: &knives::commands::release::ReapReport) -> Exit {
    if report.kept.is_empty() && report.notes.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

fn print_reap(repo: &str, report: &knives::commands::release::ReapReport) {
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
