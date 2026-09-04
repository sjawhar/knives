//! `knives release rebase`: move the whole composition onto an upstream commit.
//!
//! One ordered gate sequence — frozen pins, stale bases, the target's
//! provenance, the landed members a bare rebase sheds — around one
//! `jj rebase -b <release> -d <target>`, with the report of what moved. The
//! cut lives in `release_cut`; the membership verbs in `release_edit`.

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::release;
use knives::forge::PullRequest;
use knives::forge::github::CliForge;
use knives::ids::{ReleaseScheme, RepoName};
use knives::ledger::Ledger;
use knives::release_model::{BranchSuccessions, carried_from_tips, trunk_positions};

use super::release_edit::{EditRecord, record_edit_event, release_is_locally_movable};

/// Rebase the whole composition onto an upstream commit: `jj rebase -b <release> -d <target>`.
///
/// Every member branch's commits move onto the target and the release merge
/// moves with them, bookmarks following their rewritten commits — recorded
/// conflict resolutions replay as ordinary rebase semantics. The upstream base
/// is never a release parent; this is how the release's members change theirs.
/// A cut deliberately does not do this: which upstream commit to move onto,
/// and whether to move at all, is a judgment. After a bare rebase, members
/// whose pull requests landed and carry nothing more are dropped — the work
/// reaches the release through its new base — unless `--no-drop` keeps them.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "rebasing is one ordered stateful gate sequence; its inputs remain explicit and its stages must not be separated"
)]
pub(crate) fn run_rebase(
    fork: &Fork<'_>,
    reference: Option<&str>,
    no_drop: bool,
    extra_consumers: &[&std::path::Path],
    cache_root: Option<&std::path::Path>,
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
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let opened = knives::jj::Repo::open(path)?;
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals: &locals,
        forge: &forge,
        cache_root,
        heads: &heads,
    };
    let plan = release::plan(fork, &consumers, &Ledger::for_repo(repo).entries()?)?;
    if !plan.problems.is_empty() {
        println!("{}", release::render(&plan));
        return Ok(Exit::Incomplete);
    }
    let Some(release_name) = plan.release.clone() else {
        println!("{repo}: no release to move");
        return Ok(Exit::Ok);
    };
    if !release_is_locally_movable(&opened, repo, &release_name) {
        return Ok(Exit::Incomplete);
    }
    let Some(destination) = rebase_target(RebaseTargetInput {
        fork,
        opened: &opened,
        reference,
        cache_root,
    })?
    else {
        return Ok(Exit::Incomplete);
    };
    let onto = destination.onto.clone();
    let reference = destination.reference.clone();
    let release_commit = opened.resolve_commit(&release_name)?;
    if let Some(exit) = existing_rebase_exit(ExistingRebaseInput {
        opened: &opened,
        fork,
        release_name: &release_name,
        release_commit: &release_commit,
        destination: &destination,
        no_drop,
        bound,
    })? {
        return Ok(exit);
    }
    let scheme = entry.release_scheme();
    if let Some(exit) = frozen_rebase_exit(
        repo,
        &release_name,
        &scheme,
        release::repair_effect(
            &plan.pins,
            knives::ids::BookmarkRef::parse(&release_name).branch(),
        ),
    ) {
        return Ok(exit);
    }
    let context = RebaseContext {
        repo,
        entry,
        opened: &opened,
    };
    let Some((members, shed)) = classify_rebase_parents(&context, &release_name, &onto)? else {
        return Ok(Exit::Incomplete);
    };
    if all_landed(&opened, &members, &onto)? {
        println!(
            "{repo}: every member of {release_name} has landed in {reference}; rebasing \
             would make the trunk the only parent, so nothing moved \u{2014} reap the release \
             or include new work"
        );
        return Ok(Exit::Incomplete);
    }
    if members.is_empty() {
        println!("{repo}: {release_name} has no member parents to move; nothing to rebase");
        return Ok(Exit::Incomplete);
    }
    shed_stale_bases(path, (&release_name, &release_commit), &members, shed)?;
    knives::jj::rebase_branch_onto(path, &release_name, &onto)?;
    report_rebased_release(
        fork,
        &RebasedRelease {
            name: &release_name,
            reference: &reference,
            onto: &onto,
            shed,
        },
        bound,
    )?;
    if no_drop {
        return Ok(Exit::Ok);
    }
    drop_landed_members(fork, &release_name, &destination, bound)
}

fn frozen_rebase_exit(
    repo: &RepoName,
    release_name: &str,
    scheme: &ReleaseScheme,
    effect: release::RepairEffect,
) -> Option<Exit> {
    if effect != release::RepairEffect::NewDatedName {
        return None;
    }
    match scheme {
        ReleaseScheme::Dated => println!(
            "{repo}: every pin of {release_name} is frozen, so moving it would reach \
             nobody; cut a new dated release instead"
        ),
        ReleaseScheme::Fixed(_) => println!(
            "{repo}: every pin of {release_name} is frozen, so moving the fixed branch \
             would reach nobody; update the frozen consumer pins, or change the release \
             scheme before advancing it (fixed branches cannot reach revision pins)"
        ),
    }
    Some(Exit::Incomplete)
}

/// Rewrite the release to its member parents only, shedding stale bases.
fn shed_stale_bases(
    path: &std::path::Path,
    (release_name, release_commit): (&str, &knives::ids::CommitId),
    members: &[knives::ids::CommitId],
    shed: usize,
) -> anyhow::Result<()> {
    if shed == 0 {
        return Ok(());
    }
    knives::jj::write_release(
        path,
        &knives::jj::ReleaseWrite {
            source: Some(release_commit),
            parents: members,
            message: None,
            bookmark: Some(release_name),
            operation: &format!("knives: {release_name}: shed {shed} stale base parent(s)"),
        },
    )?;
    Ok(())
}

/// One repo mid-rebase: what parent classification needs to read and say.
struct RebaseContext<'a> {
    repo: &'a RepoName,
    entry: &'a knives::config::RepoEntry,
    opened: &'a knives::jj::Repo,
}

/// The release's moving members and the count of shed legacy base parents, or
/// `None` after printing the stale-parent refusal.
///
/// A stale parent is the old tip of a branch that has moved on: rebasing
/// carries pre-rewrite code onto the new base. A held parent — any bookmark
/// still on it whose branch is neither a release name nor the trunk — is a
/// member and moves. An unheld parent already reachable from the target is a
/// legacy trunk parent: the base is never a parent, so it is shed.
fn classify_rebase_parents(
    context: &RebaseContext<'_>,
    release_name: &str,
    onto: &knives::ids::CommitId,
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, usize)>> {
    let (repo, entry, opened) = (context.repo, context.entry, context.opened);
    let scheme = entry.release_scheme();
    let parents = opened.parents_of(release_name)?;
    let mut members: Vec<knives::ids::CommitId> = Vec::new();
    let mut shed = 0usize;
    for parent in &parents {
        let held = parent.bookmarks.iter().any(|reference| {
            !knives::ids::is_release_name(reference.branch(), &scheme)
                && reference.branch().as_str() != entry.trunk()
        });
        if held {
            members.push(parent.commit.clone());
            continue;
        }
        if opened.is_ancestor(&parent.commit, onto)? {
            shed += 1;
            continue;
        }
        let no_bookmark = if parent.bookmarks.is_empty() {
            "; no bookmark points at it"
        } else {
            ""
        };
        // The branch this parent belonged to, found by ancestry or by change
        // id, so a member rebased onto the newer trunk is named as itself and
        // the refusal says how it moves — never "fix the branch", which read as
        // an instruction to put a copy back on the old base.
        let moved = stale_parent_moved_branches(opened, entry, &parent.commit)?;
        match moved {
            Some(moved) => eprintln!(
                "{repo}: refusing to rebase {release_name}: parent {} is stale{no_bookmark}; it \
                 was {moved}. `knives release advance` moves the member to its branch, then \
                 re-run; carrying the old commit could ship pre-rewrite code.",
                parent.commit.short(),
            ),
            None => eprintln!(
                "{repo}: refusing to rebase {release_name}: parent {} is stale{no_bookmark}, and \
                 no local branch continues it. Drop it from the release (`knives release drop \
                 {}`) or restore its branch, then re-run; carrying it could ship pre-rewrite \
                 code.",
                parent.commit.short(),
                parent.commit.short(),
            ),
        }
        return Ok(None);
    }
    Ok(Some((members, shed)))
}

/// A resolved rebase destination: the commit, the label the report and
/// provenance use, and which of our pull requests the forge says landed by it.
struct RebaseDestination {
    onto: knives::ids::CommitId,
    reference: String,
    /// Empty for an explicit reference: dropping is the bare default's job,
    /// because only it knows the target covers every landing.
    landed: Vec<PullRequest>,
}

#[derive(Clone, Copy)]
struct RebaseTargetInput<'a> {
    fork: &'a Fork<'a>,
    opened: &'a knives::jj::Repo,
    reference: Option<&'a str>,
    cache_root: Option<&'a std::path::Path>,
}

#[derive(Clone, Copy)]
struct ExistingRebaseInput<'a> {
    opened: &'a knives::jj::Repo,
    fork: &'a Fork<'a>,
    release_name: &'a str,
    release_commit: &'a knives::ids::CommitId,
    destination: &'a RebaseDestination,
    no_drop: bool,
    bound: Option<&'a RepoName>,
}

fn existing_rebase_exit(input: ExistingRebaseInput<'_>) -> anyhow::Result<Option<Exit>> {
    let ExistingRebaseInput {
        opened,
        fork,
        release_name,
        release_commit,
        destination,
        no_drop,
        bound,
    } = input;
    if !opened.is_ancestor(&destination.onto, release_commit)? {
        return Ok(None);
    }
    println!(
        "{}: {release_name} already contains {}",
        fork.name, destination.reference
    );
    let exit = if no_drop {
        Exit::Ok
    } else {
        drop_landed_members(fork, release_name, destination, bound)?
    };
    Ok(Some(exit))
}

/// The commit a rebase moves onto, with the label the report and provenance use.
///
/// An explicit reference is taken at its word. Without one, the default is the
/// first upstream trunk commit that contains every merged pull request — merged,
/// not closed, because closed landed nothing.
fn rebase_target(input: RebaseTargetInput<'_>) -> anyhow::Result<Option<RebaseDestination>> {
    let RebaseTargetInput {
        fork,
        opened,
        reference,
        cache_root,
    } = input;
    if let Some(explicit) = reference {
        return Ok(Some(RebaseDestination {
            onto: opened.resolve_commit(explicit)?,
            reference: explicit.to_owned(),
            landed: Vec::new(),
        }));
    }
    merged_rebase_target(fork, opened, cache_root)
}

fn select_merged_numbers(discovery: &knives::snapshot::Discovery<'_>, trunk: &str) -> Vec<u64> {
    knives::forge::merged_onto(&discovery.ours(), trunk)
        .iter()
        .map(|pull| pull.number)
        .collect()
}

/// The bare-rebase default target, or `None` with its reason already printed.
///
/// Rebasing to this point makes every merged branch's work part of the members'
/// shared history without carrying anything upstream has not accepted. With
/// nothing merged there is no such point, and which commit to move onto goes
/// back to being a judgment the caller must make.
fn merged_rebase_target(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    cache_root: Option<&std::path::Path>,
) -> anyhow::Result<Option<RebaseDestination>> {
    let repo = &fork.name;
    let entry = fork.entry;
    let trunk = entry.upstream_trunk();
    let forge = CliForge;
    let opened_snapshot = match knives::snapshot::open(knives::snapshot::SnapshotConfig {
        forge: &forge,
        path: &fork.checkout.path,
        remotes: [
            entry.remote(knives::config::Role::Origin),
            entry.remote(knives::config::Role::Release),
        ],
        cache_root,
    }) {
        Ok(opened_snapshot) => opened_snapshot,
        Err(error) => {
            eprintln!(
                "{repo}: could not ask the forge which pull requests merged: {error}; \
                 provide a commit to rebase onto"
            );
            return Ok(None);
        }
    };
    let snapshot = match opened_snapshot.complete_with(entry.trunk(), select_merged_numbers) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!(
                "{repo}: could not ask the forge which pull requests merged: {error}; \
                 provide a commit to rebase onto"
            );
            return Ok(None);
        }
    };
    let candidates = knives::forge::merged_onto(snapshot.ours(), entry.trunk());
    let result: anyhow::Result<Option<RebaseDestination>> = (|| {
        let Some(landed) = verified_merged_candidates(repo, &snapshot, &candidates, entry.trunk())
        else {
            return Ok(None);
        };
        if landed.is_empty() {
            println!(
                "{repo}: no pull request has merged, so there is no default target; \
                 provide a commit to rebase onto"
            );
            return Ok(None);
        }
        let tip = opened.resolve_commit(&trunk)?;
        let mut placed: Vec<(u64, knives::ids::CommitId)> = Vec::new();
        let mut unplaced: Vec<u64> = Vec::new();
        for pull in &landed {
            let oid = pull.merge_commit.as_ref().map(|merge| merge.oid.clone());
            let number = pull.number;
            // Unrecorded, unresolvable and out-of-trunk merge commits are one fact
            // here: the local trunk does not carry that landing yet.
            match oid.and_then(|oid| opened.resolve_commit(&oid).ok()) {
                Some(commit) if opened.is_ancestor(&commit, &tip)? => placed.push((number, commit)),
                _ => unplaced.push(number),
            }
        }
        if !unplaced.is_empty() {
            println!(
                "{repo}: the merge commit(s) of {} are not in the local {trunk}; \
                 run knives sync, or provide a commit to rebase onto",
                numbered(&unplaced)
            );
            return Ok(None);
        }
        let Some((onto, reference)) = covering_commit(repo, opened, &placed, &trunk)? else {
            return Ok(None);
        };
        Ok(Some(RebaseDestination {
            onto,
            reference,
            landed,
        }))
    })();
    if let Err(note) = snapshot.persist(None) {
        eprintln!("{repo}: {note}");
    }
    result
}

fn verified_merged_candidates(
    repo: &RepoName,
    snapshot: &knives::snapshot::CompletedSnapshot<'_>,
    candidates: &[knives::forge::PullSummary],
    trunk: &str,
) -> Option<Vec<knives::forge::PullRequest>> {
    let unanswered: Vec<u64> = candidates
        .iter()
        .filter(|candidate| snapshot.fact(candidate.number).is_none())
        .map(|candidate| candidate.number)
        .collect();
    if !unanswered.is_empty() {
        eprintln!(
            "{repo}: could not ask the forge which pull requests merged: it did not report \
             facts for {}; provide a commit to rebase onto",
            numbered(&unanswered)
        );
        return None;
    }
    let mut landed = Vec::new();
    for candidate in candidates {
        let fact = snapshot.fact(candidate.number)?;
        let pull = &fact.pull;
        if pull.is_merged() && pull.base_ref_name.as_deref() == Some(trunk) {
            landed.push(pull.clone());
        }
    }
    Some(landed)
}

/// The first trunk commit containing every landing in `placed`: their maximum
/// by ancestry, which on a trunk is the newest of them.
fn covering_commit(
    repo: &RepoName,
    opened: &knives::jj::Repo,
    placed: &[(u64, knives::ids::CommitId)],
    trunk: &str,
) -> anyhow::Result<Option<(knives::ids::CommitId, String)>> {
    let Some(((_, first), rest)) = placed.split_first() else {
        anyhow::bail!("{repo}: no landed merge commit to cover; this is a bug");
    };
    let mut covering = first.clone();
    for (_, commit) in rest {
        if opened.is_ancestor(&covering, commit)? {
            covering = commit.clone();
        }
    }
    let numbers: Vec<u64> = placed.iter().map(|(number, _)| *number).collect();
    for (_, commit) in placed {
        if commit != &covering && !opened.is_ancestor(commit, &covering)? {
            // Merge commits that do not sit on one line cannot all be covered by
            // one of them; picking any would leave merged work out silently.
            println!(
                "{repo}: the merge commits of {} do not sit on one line of {trunk}; \
                 provide a commit to rebase onto",
                numbered(&numbers)
            );
            return Ok(None);
        }
    }
    println!(
        "{repo}: every merged pull request ({}) is in {trunk} by {}; rebasing onto it",
        numbered(&numbers),
        covering.short()
    );
    let label = covering.short().to_owned();
    Ok(Some((covering, label)))
}

/// `#7, #12` — pull requests named the way the forge shows them.
fn numbered(numbers: &[u64]) -> String {
    let numbers: Vec<String> = numbers.iter().map(|number| format!("#{number}")).collect();
    numbers.join(", ")
}

/// Whether every member has landed at or before `onto`. No members is not that:
/// it is its own refusal, with its own message.
fn all_landed(
    opened: &knives::jj::Repo,
    members: &[knives::ids::CommitId],
    onto: &knives::ids::CommitId,
) -> anyhow::Result<bool> {
    if members.is_empty() {
        return Ok(false);
    }
    for member in members {
        if !opened.is_ancestor(member, onto)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Drop the members whose pull requests landed at or before the rebase target.
///
/// Only a member carrying nothing past the target is dropped: a branch with
/// commits beyond its merged pull request still holds undelivered work, and
/// that is said instead. The drop duplicates the release onto the kept parents,
/// exactly as `drop` does, so recorded conflict resolutions carry forward.
fn drop_landed_members(
    fork: &Fork<'_>,
    release_name: &str,
    destination: &RebaseDestination,
    bound: Option<&RepoName>,
) -> anyhow::Result<Exit> {
    if destination.landed.is_empty() {
        return Ok(Exit::Ok);
    }
    let repo = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let opened = knives::jj::Repo::open(path)?;
    let parents = opened.parent_commits(release_name)?;
    let mut kept = parents.clone();
    let mut deltas: Vec<String> = Vec::new();
    for pull in &destination.landed {
        let Some(tip) = opened.local_bookmark_tip(&pull.head_ref_name) else {
            continue;
        };
        if !parents.contains(&tip) {
            continue;
        }
        if knives::jj::carries_work_past(path, &destination.onto, &tip)? {
            println!(
                "{repo}: kept {}: it carries work past #{}",
                pull.head_ref_name, pull.number
            );
            continue;
        }
        kept.retain(|parent| parent != &tip);
        deltas.push(format!(
            "dropped {}: landed upstream as #{}",
            pull.head_ref_name, pull.number
        ));
    }
    if deltas.is_empty() {
        return Ok(Exit::Ok);
    }
    if kept.is_empty() {
        println!(
            "{repo}: every member of {release_name} landed; dropping them all would leave it \
             without a parent, so nothing was dropped"
        );
        return Ok(Exit::Incomplete);
    }
    let release = opened.resolve_commit(release_name)?;
    let provenance = release::parent_sources(&opened, entry, &entry.release_scheme(), &kept)?;
    let delta = deltas.join("; ");
    let message = release::composition_message(release_name, &provenance, &delta);
    let created = knives::jj::write_release(
        path,
        &knives::jj::ReleaseWrite {
            source: Some(&release),
            parents: &kept,
            message: Some(&message),
            bookmark: Some(release_name),
            operation: &format!("knives: {release_name}: {delta}"),
        },
    )?;
    record_edit_event(
        fork,
        &opened,
        &EditRecord {
            release: release_name,
            delta: &delta,
            created: &created,
            provenance: &provenance,
        },
        bound,
    )?;
    println!(
        "{repo}: {release_name} now has {} parent(s): {delta}",
        kept.len()
    );
    match knives::jj::conflicted_files(path, created.as_str()) {
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(Exit::Ok)
}

/// A composition rebase that just happened: what moved, and onto what.
struct RebasedRelease<'a> {
    name: &'a str,
    reference: &'a str,
    onto: &'a knives::ids::CommitId,
    shed: usize,
}

/// Re-describe the rebased release so the recorded provenance names the
/// rewritten parents, and report what moved.
fn report_rebased_release(
    fork: &Fork<'_>,
    rebased: &RebasedRelease<'_>,
    bound: Option<&RepoName>,
) -> anyhow::Result<()> {
    let repo = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let reopened = knives::jj::Repo::open(path)?;
    let created = reopened.resolve_commit(rebased.name)?;
    let new_parents = reopened.parent_commits(rebased.name)?;
    let provenance =
        release::parent_sources(&reopened, entry, &entry.release_scheme(), &new_parents)?;
    let delta = format!("rebased onto {}", rebased.reference);
    let message = release::composition_message(rebased.name, &provenance, &delta);
    let described = knives::jj::describe_commit(
        path,
        &created,
        &message,
        &format!("knives: {}: record rebased provenance", rebased.name),
    )?;
    // A rebase rewrites every member's commit id; without this the ledger's
    // last (branch, commit) pairing for the release would name commits no
    // longer among its parents, and the next edit could not tell a rebuilt
    // branch from a stranger.
    record_edit_event(
        fork,
        &reopened,
        &EditRecord {
            release: rebased.name,
            delta: &delta,
            created: &described,
            provenance: &provenance,
        },
        bound,
    )?;
    let stale_bases = if rebased.shed > 0 {
        format!(", {} stale base parent(s) shed", rebased.shed)
    } else {
        String::new()
    };
    println!(
        "{repo}: {} rebased onto {} ({}), {} member(s) moved with it{stale_bases}",
        rebased.name,
        rebased.reference,
        rebased.onto.short(),
        new_parents.len()
    );
    match knives::jj::conflicted_files(path, described.as_str()) {
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(())
}

/// `feat/alpha (now 1a2b3c4d5e6f)` for every maintained branch that continues a
/// stale parent — grown past it or rebased off it — or `None` when none does.
fn stale_parent_moved_branches(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    parent: &knives::ids::CommitId,
) -> Result<Option<String>, knives::jj::JjError> {
    let branches = carried_from_tips(
        &opened.bookmark_tips()?,
        entry.trunk(),
        &entry.release_scheme(),
    );
    let trunks = trunk_positions(opened, entry)?;
    let moved: Vec<String> = BranchSuccessions::of(opened, &trunks, &branches)?
        .successors_of(parent)?
        .into_iter()
        .map(|(branch, tip)| format!("{branch} (now {})", tip.short()))
        .collect();
    Ok((!moved.is_empty()).then(|| moved.join(", ")))
}
