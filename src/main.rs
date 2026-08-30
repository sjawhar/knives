//! The `knives` binary.
//!
//! Dispatch, plus the release-edit verbs: `include`, `drop` and `advance` decide
//! here because each is one jj sequence over a parent set rather than a report
//! with a renderer. Every other command owns its own logic and returns an
//! [`Exit`], so the match stays a table.
// allow: SIZE_OK: 2333 lines - dispatch plus the release-edit verbs; splitting would scatter the exhaustive match.

use std::process::ExitCode;

use clap::Parser as _;
use knives::cli::{Cli, Command, Exit, ReleaseAction};
use knives::commands::{
    hook, init, notch, preflight, register, release, repos, start, status, sync,
};
use knives::config::{default_config_path, load};
use knives::detect::RebaseOutcome;
use knives::forge::{CliForge, Forge, PullRequest};
use knives::ids::{BranchName, BranchTarget, ReleaseScheme, RepoName, Requirement};
use knives::ledger::{Draft, Kind, Ledger, Scribe};
use knives::store::{Store, default_state_path};

fn main() -> ExitCode {
    match dispatch() {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(error) => {
            eprintln!("knives: {error:#}");
            ExitCode::from(Exit::Incomplete.code())
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive command dispatch is easier to audit as one table"
)]
fn dispatch() -> anyhow::Result<Exit> {
    let cli = Cli::parse();
    let output = knives::cli::output_format(cli.json, cli.text);
    match cli.command {
        Command::Hook { harness } => Ok(hook::run(harness)),
        Command::Init { repo } => init::run(repo),
        Command::Register { repo } => register::run(repo),
        Command::Repos => repos::run(),
        Command::Status {
            repo,
            all,
            verbose,
            no_landed,
            no_github,
        } => run_status(
            repo.as_deref(),
            StatusView {
                scope: Scope { all },
                gather: Gather {
                    probe: !no_landed,
                    use_forge: !no_github,
                },
                display: Display { verbose, output },
            },
        ),
        Command::Sync {
            repo,
            all,
            no_github,
        } => run_sync(repo.as_deref(), all, output, !no_github),
        Command::Start { branch, repo, why } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            start::run(&name, &BranchName::new(branch), why.as_deref())
        }
        Command::Finish {
            branch,
            repo,
            no_cleanup,
            superseded_by,
            allow_open,
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_finish(
                &BranchTarget::new(name, BranchName::new(branch)),
                superseded_by.as_deref(),
                !no_cleanup,
                allow_open,
            )
        }
        Command::Track {
            branch,
            pr,
            fork_only,
            forget,
            repo,
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_track(
                &BranchTarget::new(name, BranchName::new(branch)),
                pr,
                fork_only,
                forget,
            )
        }
        Command::Depends { branch, on, repo } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_depends(&BranchTarget::new(name, BranchName::new(branch)), &on)
        }
        Command::Notch {
            subject,
            message,
            evidence,
            pr,
            repo,
        } => {
            if subject
                .as_deref()
                .is_some_and(|name| name.trim().is_empty())
            {
                eprintln!("subject cannot be empty");
                return Ok(Exit::Usage);
            }
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            notch::run(
                &notch::Request {
                    repo: &name,
                    subject: subject.as_deref(),
                    message: message.as_deref(),
                    evidence: &evidence,
                    pr,
                },
                output,
            )
        }
        Command::Preflight { repo } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_preflight(name.as_str())
        }
        Command::Release {
            action,
            repo,
            consumer,
        } => {
            let Some(chosen) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            let extra: Vec<&std::path::Path> =
                consumer.iter().map(std::path::PathBuf::as_path).collect();
            dispatch_release(&chosen, action, &extra)
        }
        Command::Gh { args } => match knives::commands::gh::run(&args)? {},
    }
}

/// The one repo a command acts on: named, or inferred from where you are standing.
///
/// Requiring the name on every command is absurd when you are inside the repository,
/// and it was the loudest complaint about using this thing.
fn one_repo(requested: Option<&str>) -> anyhow::Result<Option<RepoName>> {
    if let Some(name) = requested {
        let registry = load(&default_config_path())?;
        if registry.get(&RepoName::new(name)).is_none() {
            let known: Vec<String> = registry.names().map(|n| n.to_string()).collect();
            eprintln!("unknown repo {name}; known: {}", known.join(", "));
            return Ok(None);
        }
        return Ok(Some(RepoName::new(name)));
    }
    let registry = load(&default_config_path())?;
    let here = std::env::current_dir()?;
    if let Some((name, _)) = registry.containing(&here) {
        Ok(Some(name))
    } else {
        eprintln!(
            "not inside a managed repo; name one, or run this from inside it. known: {}",
            registry
                .names()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(None)
    }
}

/// Plan, curate or cut a release.
fn dispatch_release(
    chosen: &RepoName,
    action: Option<ReleaseAction>,
    extra_consumers: &[&std::path::Path],
) -> anyhow::Result<Exit> {
    match action {
        None => run_release(chosen.as_str(), extra_consumers, &ReleaseInvocation::Plan),
        Some(ReleaseAction::Cut { name, allow_drop }) => run_release(
            chosen.as_str(),
            extra_consumers,
            &ReleaseInvocation::Cut { name, allow_drop },
        ),
        Some(ReleaseAction::Rebase { reference, no_drop }) => {
            let cache_root = knives::forge_cache::cache_root();
            run_rebase(
                chosen.as_str(),
                reference.as_deref(),
                no_drop,
                cache_root.as_deref(),
            )
        }
        Some(ReleaseAction::Carries { revision, target }) => {
            let registry = load(&default_config_path())?;
            let Some(entry) = registry.get(chosen) else {
                return Ok(Exit::Usage);
            };
            run_release_carries(chosen, entry, &revision, target.as_deref())
        }
        Some(ReleaseAction::Reap) => run_reap(chosen.as_str()),
        Some(ReleaseAction::Include { branch, why }) => {
            run_release_edit(chosen.as_str(), &ReleaseEdit::Include { branch, why })
        }
        Some(ReleaseAction::Drop { branch, why }) => {
            run_release_edit(chosen.as_str(), &ReleaseEdit::Drop { branch, why })
        }
        Some(ReleaseAction::Advance { branches, from }) => {
            let branches = branches.into_iter().map(BranchName::new).collect();
            run_release_edit(chosen.as_str(), &ReleaseEdit::Advance { branches, from })
        }
    }
}

/// Answer "does <target> carry <revision>" with the replay test, not text search.
fn run_release_carries(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    revision: &str,
    target: Option<&str>,
) -> anyhow::Result<Exit> {
    let named = match target {
        Some(reference) => reference.to_owned(),
        None => {
            if let Some(name) = release::plan(repo, entry, &entry.consumers)?.release {
                name
            } else {
                println!("{repo}: no release to check against; cut one or pass --in <ref>");
                return Ok(Exit::Incomplete);
            }
        }
    };
    let outcome =
        knives::jj::probe_landed(&entry.path, &knives::ids::BranchName::new(revision), &named)?;
    Ok(match outcome {
        RebaseOutcome::Empty => {
            println!("{repo}: {revision} is carried in {named}: replaying it leaves nothing");
            Exit::Ok
        }
        RebaseOutcome::CleanNonEmpty => {
            println!(
                "{repo}: {revision} is NOT carried in {named}: replaying it leaves real diffs"
            );
            Exit::Findings
        }
        RebaseOutcome::Conflicted => {
            println!(
                "{repo}: {revision} conflicts with {named}: some of its content is there, \
                 or unrelated work touched the same files; judge it by eye"
            );
            Exit::Findings
        }
    })
}

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
fn run_rebase(
    name: &str,
    reference: Option<&str>,
    no_drop: bool,
    cache_root: Option<&std::path::Path>,
) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut worst = Exit::Ok;
    for (repo, entry) in chosen {
        let opened = knives::jj::Repo::open(&entry.path)?;
        let plan = release::plan(&repo, &entry, &entry.consumers)?;
        let Some(release_name) = plan.release.clone() else {
            println!("{repo}: no release to move");
            continue;
        };
        if !release_is_locally_movable(&opened, &repo, &release_name)? {
            worst = worst.worst(Exit::Incomplete);
            continue;
        }
        let Some(destination) = rebase_target(RebaseTargetInput {
            repo: &repo,
            entry: &entry,
            opened: &opened,
            reference,
            cache_root,
        })?
        else {
            worst = worst.worst(Exit::Incomplete);
            continue;
        };
        let onto = destination.onto.clone();
        let reference = destination.reference.clone();
        let release_commit = opened.resolve_commit(&release_name)?;
        if let Some(exit) = existing_rebase_exit(ExistingRebaseInput {
            opened: &opened,
            repo: &repo,
            entry: &entry,
            release_name: &release_name,
            release_commit: &release_commit,
            destination: &destination,
            no_drop,
        })? {
            worst = worst.worst(exit);
            continue;
        }
        let scheme = entry.release_scheme();
        if let Some(exit) = frozen_rebase_exit(
            &repo,
            &release_name,
            &scheme,
            release::repair_effect(&plan.pins),
        ) {
            worst = worst.worst(exit);
            continue;
        }
        let context = RebaseContext {
            repo: &repo,
            entry: &entry,
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
            worst = worst.worst(Exit::Incomplete);
            continue;
        }
        if members.is_empty() {
            println!("{repo}: {release_name} has no member parents to move; nothing to rebase");
            worst = worst.worst(Exit::Incomplete);
            continue;
        }
        shed_stale_bases(&entry, (&release_name, &release_commit), &members, shed)?;
        knives::jj::rebase_branch_onto(&entry.path, &release_name, &onto)?;
        report_rebased_release(
            &repo,
            &entry,
            &RebasedRelease {
                name: &release_name,
                reference: &reference,
                onto: &onto,
                shed,
            },
        )?;
        if !no_drop {
            worst = worst.worst(drop_landed_members(
                &repo,
                &entry,
                &release_name,
                &destination,
            )?);
        }
    }
    Ok(worst)
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
    entry: &knives::config::RepoEntry,
    (release_name, release_commit): (&str, &knives::ids::CommitId),
    members: &[knives::ids::CommitId],
    shed: usize,
) -> anyhow::Result<()> {
    if shed == 0 {
        return Ok(());
    }
    knives::jj::write_release(
        &entry.path,
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
    let tips = opened.bookmark_tips()?;
    let mut members: Vec<knives::ids::CommitId> = Vec::new();
    let mut shed = 0usize;
    for parent in &parents {
        let held = parent.bookmarks.iter().any(|reference| {
            tips.get(reference) == Some(&parent.commit)
                && !knives::ids::is_release_name(reference.branch(), &scheme)
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
        let moved = stale_parent_moved_branches(entry, &scheme, &parent.commit)?;
        let moved = moved.map_or_else(String::new, |moved| format!("; moved tip(s): {moved}"));
        eprintln!(
            "{repo}: refusing to rebase {release_name}: parent {} is stale{no_bookmark}{moved}. \
             Fix the branch or drop it from the release, then re-run; carrying it could ship \
             pre-rewrite code.",
            short12(&parent.commit),
        );
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
    repo: &'a RepoName,
    entry: &'a knives::config::RepoEntry,
    opened: &'a knives::jj::Repo,
    reference: Option<&'a str>,
    cache_root: Option<&'a std::path::Path>,
}

#[derive(Clone, Copy)]
struct ExistingRebaseInput<'a> {
    opened: &'a knives::jj::Repo,
    repo: &'a RepoName,
    entry: &'a knives::config::RepoEntry,
    release_name: &'a str,
    release_commit: &'a knives::ids::CommitId,
    destination: &'a RebaseDestination,
    no_drop: bool,
}

fn existing_rebase_exit(input: ExistingRebaseInput<'_>) -> anyhow::Result<Option<Exit>> {
    let ExistingRebaseInput {
        opened,
        repo,
        entry,
        release_name,
        release_commit,
        destination,
        no_drop,
    } = input;
    if !opened.is_ancestor(&destination.onto, release_commit)? {
        return Ok(None);
    }
    println!(
        "{repo}: {release_name} already contains {}",
        destination.reference
    );
    let exit = if no_drop {
        Exit::Ok
    } else {
        drop_landed_members(repo, entry, release_name, destination)?
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
        repo,
        entry,
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
    merged_rebase_target(repo, entry, opened, cache_root)
}

/// The bare-rebase default target, or `None` with its reason already printed.
///
/// Rebasing to this point makes every merged branch's work part of the members'
/// shared history without carrying anything upstream has not accepted. With
/// nothing merged there is no such point, and which commit to move onto goes
/// back to being a judgment the caller must make.
fn merged_rebase_target(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    opened: &knives::jj::Repo,
    cache_root: Option<&std::path::Path>,
) -> anyhow::Result<Option<RebaseDestination>> {
    let trunk = entry.upstream_trunk();
    let forge = CliForge;
    let opened_snapshot = match knives::snapshot::open(knives::snapshot::SnapshotConfig {
        forge: &forge,
        path: &entry.path,
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
    let discovery = match opened_snapshot.discover() {
        Ok(discovery) => discovery,
        Err(error) => {
            eprintln!(
                "{repo}: could not ask the forge which pull requests merged: {error}; \
                 provide a commit to rebase onto"
            );
            return Ok(None);
        }
    };
    let numbers: Vec<u64> = knives::forge::merged_onto(&discovery.ours(), entry.trunk())
        .iter()
        .map(|pull| pull.number)
        .collect();
    let snapshot = match discovery.complete(&numbers) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!(
                "{repo}: could not ask the forge which pull requests merged: {error}; \
                 provide a commit to rebase onto"
            );
            return Ok(None);
        }
    };
    let candidates = knives::forge::merged_onto(&snapshot.ours(), entry.trunk());
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
    if let Err(error) = snapshot.persist(None) {
        eprintln!("{repo}: could not update forge cache: {error}");
    }
    result
}

fn verified_merged_candidates(
    repo: &RepoName,
    snapshot: &knives::snapshot::ForgeSnapshot<'_>,
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
        if pull.is_merged() && pull.base_ref_name == trunk {
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
        short12(&covering)
    );
    let label = short12(&covering);
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
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    release_name: &str,
    destination: &RebaseDestination,
) -> anyhow::Result<Exit> {
    if destination.landed.is_empty() {
        return Ok(Exit::Ok);
    }
    let opened = knives::jj::Repo::open(&entry.path)?;
    let parents: Vec<knives::ids::CommitId> = opened
        .parents_of(release_name)?
        .into_iter()
        .map(|parent| parent.commit)
        .collect();
    let mut kept = parents.clone();
    let mut deltas: Vec<String> = Vec::new();
    for pull in &destination.landed {
        let Some(tip) = bookmark_tip(&opened, &pull.head_ref_name)? else {
            continue;
        };
        if !parents.contains(&tip) {
            continue;
        }
        if knives::jj::carries_work_past(&entry.path, &destination.onto, &tip)? {
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
    let provenance = parent_sources(&opened, entry, &entry.release_scheme(), &kept)?;
    let delta = deltas.join("; ");
    let message = format!(
        "{}\n\n{delta}",
        cut_request(release_name.to_owned(), &provenance).message()
    );
    let created = knives::jj::write_release(
        &entry.path,
        &knives::jj::ReleaseWrite {
            source: Some(&release),
            parents: &kept,
            message: Some(&message),
            bookmark: Some(release_name),
            operation: &format!("knives: {release_name}: {delta}"),
        },
    )?;
    println!(
        "{repo}: {release_name} now has {} parent(s): {delta}",
        kept.len()
    );
    match knives::jj::conflicted_files(&entry.path, created.as_str()) {
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
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    rebased: &RebasedRelease<'_>,
) -> anyhow::Result<()> {
    let reopened = knives::jj::Repo::open(&entry.path)?;
    let created = reopened.resolve_commit(rebased.name)?;
    let new_parents: Vec<knives::ids::CommitId> = reopened
        .parents_of(rebased.name)?
        .into_iter()
        .map(|parent| parent.commit)
        .collect();
    let provenance = parent_sources(&reopened, entry, &entry.release_scheme(), &new_parents)?;
    let message = format!(
        "{}\n\nrebased onto {}",
        cut_request(rebased.name.to_owned(), &provenance).message(),
        rebased.reference
    );
    let described = knives::jj::describe_commit(
        &entry.path,
        &created,
        &message,
        &format!("knives: {}: record rebased provenance", rebased.name),
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
        short12(rebased.onto),
        new_parents.len()
    );
    match knives::jj::conflicted_files(&entry.path, described.as_str()) {
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(())
}

fn stale_parent_moved_branches(
    entry: &knives::config::RepoEntry,
    scheme: &ReleaseScheme,
    parent: &knives::ids::CommitId,
) -> Result<Option<String>, knives::jj::JjError> {
    let moved = knives::jj::branches_past(&entry.path, parent)?;
    let moved: Vec<String> = moved
        .into_iter()
        .filter(|(branch, _)| {
            !knives::ids::is_release_name(branch, scheme) && branch.as_str() != "@git"
        })
        .map(|(branch, tip)| format!("{branch} (now {})", short12(&tip)))
        .collect();
    Ok((!moved.is_empty()).then(|| moved.join(", ")))
}

/// One deliberate change to the release in hand.
enum ReleaseEdit {
    Include {
        branch: String,
        why: Option<String>,
    },
    Drop {
        branch: String,
        why: String,
    },
    Advance {
        branches: Vec<BranchName>,
        from: Option<String>,
    },
}

/// What an edit decided: a new parent set to write with the delta that describes
/// it, or an already-reported end.
enum EditOutcome {
    Done(Vec<knives::ids::CommitId>, String),
    Settled(Exit),
}

/// The release in hand: a flat merge of feature and fix branches.
///
/// Every parent is a member. The upstream base is never a direct parent — it
/// is reachable through every member, because members fork from it — so there
/// is no role to classify and nothing for a landed member to be mistaken for.
struct ReleaseInHand {
    name: String,
    commit: knives::ids::CommitId,
    parents: Vec<knives::ids::CommitId>,
    /// Where the trunk sits, upstream and locally. The trunk is not a feature
    /// branch, so neither position is ever a member.
    trunk_tips: Vec<knives::ids::CommitId>,
}

impl ReleaseInHand {
    /// Read the release's parents: the members.
    fn read(
        opened: &knives::jj::Repo,
        entry: &knives::config::RepoEntry,
        name: String,
        trunk_tip: knives::ids::CommitId,
    ) -> anyhow::Result<Self> {
        let commit = opened.resolve_commit(&name)?;
        let parents: Vec<knives::ids::CommitId> = opened
            .parents_of(&name)?
            .into_iter()
            .map(|parent| parent.commit)
            .collect();
        let mut trunk_tips = vec![trunk_tip];
        if let Some(local) = bookmark_tip(opened, entry.trunk())? {
            trunk_tips.push(local);
        }
        Ok(Self {
            name,
            commit,
            parents,
            trunk_tips,
        })
    }

    /// The member parents: all of them.
    fn members(&self) -> impl Iterator<Item = &knives::ids::CommitId> {
        self.parents.iter()
    }
}

/// Everything an edit reads: whose release, which repository, which release.
struct EditContext<'a> {
    repo: &'a RepoName,
    opened: &'a knives::jj::Repo,
    release: &'a ReleaseInHand,
}

/// Apply one stated change to each chosen repo's release in hand.
fn run_release_edit(name: &str, change: &ReleaseEdit) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut worst = Exit::Ok;
    for (repo, entry) in chosen {
        worst = worst.worst(edit_release(&repo, &entry, change)?);
    }
    Ok(worst)
}

/// Edit the release in hand: one change, nothing else moves.
///
/// The whole command is the jj sequence agents fumble — duplicate the release
/// onto the changed parent set, describe it, move its name — with the same pin
/// gate a rebase has. The duplicate preserves recorded conflict resolutions;
/// only the change itself can surface new conflicts, and they are reported.
fn edit_release(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    change: &ReleaseEdit,
) -> anyhow::Result<Exit> {
    let opened = knives::jj::Repo::open(&entry.path)?;
    let plan = release::plan(repo, entry, &entry.consumers)?;
    let Some(release_name) = plan.release.clone() else {
        println!("{repo}: no release to edit; cut one first");
        return Ok(Exit::Incomplete);
    };
    // Follows from who pins it, exactly as for a rebase: a consumer that follows
    // the branch sees the edit, one frozen on a revision does not. The way out
    // differs by scheme, because a fixed branch cannot take a dated name.
    if release::repair_effect(&plan.pins) == release::RepairEffect::NewDatedName {
        match entry.release_scheme() {
            ReleaseScheme::Dated => println!(
                "{repo}: every pin of {release_name} is frozen, so editing it would reach \
                 nobody; cut a new dated release first"
            ),
            ReleaseScheme::Fixed(_) => println!(
                "{repo}: every pin of {release_name} is frozen, so editing the fixed branch \
                 would reach nobody; update the frozen consumer pins, or change the release \
                 scheme before editing it (fixed branches cannot reach revision pins)"
            ),
        }
        return Ok(Exit::Incomplete);
    }
    // Fail closed: parents are classified against the upstream trunk, and an
    // unresolvable trunk would make every base look like a member — include
    // would refuse for the wrong reason, and drop or advance could touch the
    // base, which is `rebase`'s domain.
    let Ok(trunk_tip) = opened.resolve_commit(&entry.upstream_trunk()) else {
        println!(
            "{repo}: cannot resolve {}; release edits classify parents against the upstream \
             trunk, so fetch upstream first",
            entry.upstream_trunk()
        );
        return Ok(Exit::Incomplete);
    };
    if !release_is_locally_movable(&opened, repo, &release_name)? {
        return Ok(Exit::Incomplete);
    }
    let release = ReleaseInHand::read(&opened, entry, release_name, trunk_tip)?;
    let context = EditContext {
        repo,
        opened: &opened,
        release: &release,
    };
    let outcome = match change {
        ReleaseEdit::Include { branch, why } => include_edit(&context, branch, why.as_deref())?,
        ReleaseEdit::Drop { branch, why } => drop_edit(&context, branch, why)?,
        ReleaseEdit::Advance { branches, from } => {
            advance_edit(&context, entry, branches, from.as_deref())?
        }
    };
    let (new_parents, delta) = match outcome {
        EditOutcome::Settled(exit) => return Ok(exit),
        EditOutcome::Done(parents, delta) => (parents, delta),
    };
    // Built through `cut_request` so an edited release's description reads exactly
    // like a fresh cut's, from the same (source, commit) pairs.
    let provenance = parent_sources(&opened, entry, &entry.release_scheme(), &new_parents)?;
    let message = format!(
        "{}\n\n{delta}",
        cut_request(release.name.clone(), &provenance).message()
    );
    let created = knives::jj::write_release(
        &entry.path,
        &knives::jj::ReleaseWrite {
            source: Some(&release.commit),
            parents: &new_parents,
            message: Some(&message),
            bookmark: Some(&release.name),
            operation: &format!("knives: {}: {delta}", release.name),
        },
    )?;
    println!(
        "{repo}: {} now has {} parent(s): {delta}",
        release.name,
        new_parents.len()
    );
    match knives::jj::conflicted_files(&entry.path, created.as_str()) {
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(Exit::Ok)
}

/// Add one parent. Nothing else moves: an advanced member is `advance`'s job.
/// Whether the release name has one local position to move, saying why not when
/// it has none.
///
/// Editing and rebasing both move a local bookmark. A release held only as a
/// remote-tracking ref — what a fetch of somebody else's cut leaves, since jj
/// creates no local bookmark for an untracked remote one — and one whose local
/// bookmark is divergent both lack a single local position. jj rejects
/// `name@remote` as a bookmark name outright, and it did so only after the
/// duplicate had been made and described.
fn release_is_locally_movable(
    opened: &knives::jj::Repo,
    repo: &RepoName,
    name: &str,
) -> anyhow::Result<bool> {
    if bookmark_tip(opened, name)?.is_some() {
        return Ok(true);
    }
    match name.split_once('@') {
        Some((branch, remote)) => println!(
            "{repo}: {name} is here only as a remote ref, so there is no local bookmark to \
             move; `jj bookmark track {branch}@{remote}` first"
        ),
        None => println!(
            "{repo}: {name} has no single local position, so there is nothing to move; \
             resolve its divergence first"
        ),
    }
    Ok(false)
}

fn include_edit(
    context: &EditContext<'_>,
    target: &str,
    why: Option<&str>,
) -> anyhow::Result<EditOutcome> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let tip = bookmark_tip(opened, target)?.or_else(|| opened.resolve_commit(target).ok());
    let Some(tip) = tip else {
        println!("{repo}: {target} is neither a local bookmark nor a resolvable revision");
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    };
    if release.parents.contains(&tip) {
        println!("{repo}: {} already carries {target}", release.name);
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    if opened.is_ancestor(&release.commit, &tip)? {
        // Built on top of the release, not off the trunk. Including it would put
        // the cut in its own successor's ancestry, and it carries the whole
        // release with it rather than one branch's work.
        println!(
            "{repo}: {target} is stacked on {}, so it is not a member of it; rebase it off \
             the trunk to include it",
            release.name
        );
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    if opened.is_ancestor(&tip, &release.commit)? {
        // Reachable through another parent's history — a stacked branch, or one
        // whose content landed in the base. Membership is the parent set, so
        // this is not a member; say which situation holds rather than
        // pretending the include happened.
        println!(
            "{repo}: {target}'s content is already in {} through another parent's history; \
             it is not a member parent itself, and whichever parent carries it represents it",
            release.name
        );
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    if release.trunk_tips.contains(&tip) {
        // A release is a flat merge of feature and fix branches; upstream enters
        // through the members' bases, never as a parent. Checked before the
        // member scan below, because a member that landed upstream is an
        // ancestor of the trunk and would otherwise answer for it.
        println!(
            "{repo}: {target} is the trunk, not a feature or fix branch; {} carries \
             upstream through its members' bases, never as a parent",
            release.name
        );
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    for parent in release.members() {
        if opened.is_ancestor(parent, &tip)? {
            println!(
                "{repo}: {} carries {} of {target}, and the branch has advanced; moving a \
                 member is its own decision: `knives release advance {target}`",
                release.name,
                short12(parent)
            );
            return Ok(EditOutcome::Settled(Exit::Incomplete));
        }
    }
    let mut parents = release.parents.clone();
    parents.push(tip);
    let why = why.map_or_else(String::new, |why| format!(" ({why})"));
    let delta = format!("included {target}{why}");
    Ok(EditOutcome::Done(parents, delta))
}

/// Remove one member parent. The branch and its bookmark are untouched.
fn drop_edit(context: &EditContext<'_>, target: &str, why: &str) -> anyhow::Result<EditOutcome> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let mut candidates = Vec::new();
    if let Some(tip) = bookmark_tip(opened, target)? {
        if release.parents.contains(&tip) {
            // The bookmark sits exactly on a parent: that parent is the branch.
            candidates.push(tip);
        } else {
            // Ancestry is the fallback for a branch that has advanced past its
            // released parent. It can be ambiguous — a parent whose history the
            // branch shares also matches — and ambiguity refuses below.
            for parent in release.members() {
                if opened.is_ancestor(parent, &tip)? {
                    candidates.push(parent.clone());
                }
            }
        }
    } else if let Ok(commit) = opened.resolve_commit(target) {
        if release.parents.contains(&commit) {
            candidates.push(commit);
        }
    } else {
        println!("{repo}: {target} is neither a local bookmark nor a resolvable revision");
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    match candidates.as_slice() {
        [] => {
            println!(
                "{repo}: {} carries no parent of {target}; name the parent's commit id",
                release.name
            );
            Ok(EditOutcome::Settled(Exit::Incomplete))
        }
        [parent] => {
            let mut parents = release.parents.clone();
            parents.retain(|kept| kept != parent);
            // A drop only removes the parent; whether the content survives
            // depends on the remaining members' ancestry. Losing it is often
            // the point (a bad fix) and sometimes a surprise (a landed pull
            // the members' bases do not reach yet), so the fact is stated
            // rather than judged.
            let mut carried_elsewhere = false;
            for kept in &parents {
                if opened.is_ancestor(parent, kept)? {
                    carried_elsewhere = true;
                    break;
                }
            }
            if !carried_elsewhere {
                println!(
                    "{repo}: no remaining member carries {target}'s content; the release \
                     loses it"
                );
            }
            let delta = format!("dropped {target}: {why}");
            Ok(EditOutcome::Done(parents, delta))
        }
        many => {
            let listed: Vec<String> = many.iter().map(short12).collect();
            println!(
                "{repo}: {target} matches {} parents of {} ({}); name one by commit id",
                many.len(),
                release.name,
                listed.join(", ")
            );
            Ok(EditOutcome::Settled(Exit::Incomplete))
        }
    }
}

/// Move member parents to their branches' current tips, and only those named.
///
/// Bare `advance` is parent-driven: every member whose branch moved on. Named
/// branches are strict — asking to advance something that is not a member is
/// answered, not improvised around. Both work from the same population, the
/// branches a release can carry: the trunk and our release names are not among
/// them, and a member advanced onto either would leave the release carrying a
/// second base or its own predecessor.
fn advance_edit(
    context: &EditContext<'_>,
    entry: &knives::config::RepoEntry,
    branches: &[BranchName],
    from: Option<&str>,
) -> anyhow::Result<EditOutcome> {
    let tips = context.opened.bookmark_tips()?;
    let carried = release::carried_from_tips(&tips, entry.trunk(), &entry.release_scheme());
    let outcome = if let Some(from) = from {
        let [branch] = branches else {
            println!(
                "{}: --from names the old commit one branch replaces; give exactly one branch",
                context.repo
            );
            return Ok(EditOutcome::Settled(Exit::Usage));
        };
        let Ok(old) = context.opened.resolve_commit(from) else {
            println!("{}: cannot resolve {from}", context.repo);
            return Ok(EditOutcome::Settled(Exit::Incomplete));
        };
        advance_named_member_from(context, &carried, branch, &old)?
    } else if branches.is_empty() {
        advance_every_member(context, &carried)?
    } else {
        advance_named_members(context, &carried, branches)?
    };
    let Some((parents, moved)) = outcome else {
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    };
    if moved.is_empty() {
        // Only a bare advance looked at every member, so only it can say so; a
        // named advance has already reported each branch it found at its tip.
        if branches.is_empty() {
            println!(
                "{}: every member of {} is at its branch tip",
                context.repo, context.release.name
            );
        }
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    // Stacked members advanced to one tip collapse into a single parent.
    let mut deduped = Vec::new();
    for parent in parents {
        if !deduped.contains(&parent) {
            deduped.push(parent);
        }
    }
    let delta = format!("advanced {}", moved.join(", "));
    Ok(EditOutcome::Done(deduped, delta))
}

/// Advance every member whose branch has moved on. `None` is a refusal that
/// has already said why.
///
/// Two-phase: every move and every ambiguity is found first, and any ambiguity
/// refuses the whole edit. A bare `advance` promises "every advanced member",
/// and advancing some while skipping others would deliver a composition nobody
/// asked for while reporting success.
fn advance_every_member(
    context: &EditContext<'_>,
    carried: &[(String, knives::ids::CommitId)],
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, Vec<String>)>> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    // A branch built on top of the release descends from every member, so
    // ancestry alone would call it their advanced tip. Advancing onto one folds
    // work nobody included into the release and puts the cut in its own
    // successor's ancestry, so it is not a candidate at all.
    let mut off_release: Vec<(String, knives::ids::CommitId)> = Vec::new();
    for (branch, tip) in carried {
        if !opened.is_ancestor(&release.commit, tip)? {
            off_release.push((branch.clone(), tip.clone()));
        }
    }
    let mut parents = release.parents.clone();
    let mut advances: Vec<(usize, String, knives::ids::CommitId)> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        let mut successors = Vec::new();
        for (branch, tip) in &off_release {
            if tip != parent && opened.is_ancestor(parent, tip)? {
                successors.push((branch.clone(), tip.clone()));
            }
        }
        match successors.as_slice() {
            [] => {}
            [(branch, tip)] => advances.push((index, branch.clone(), tip.clone())),
            many => {
                let names: Vec<String> = many.iter().map(|(branch, _)| branch.clone()).collect();
                ambiguous.push(format!(
                    "parent {} has several advanced branches ({})",
                    short12(parent),
                    names.join(", ")
                ));
            }
        }
    }
    if !ambiguous.is_empty() {
        for line in &ambiguous {
            println!("{repo}: {line}");
        }
        println!("{repo}: nothing advanced; advance the ambiguous members by name");
        return Ok(None);
    }
    // The mirror image of the ambiguity above: one branch found as the sole
    // successor of more than one stale parent is not evidence it replaced all
    // of them. A branch stacked across several former members' tips (an
    // integration branch built across them, or a member rebuilt with `jj
    // duplicate` that left an unrelated branch still reachable from its old
    // tip) satisfies the ancestry check for each parent individually, and
    // deduping the result would silently fold distinct members into one,
    // discarding a match nobody asked for. Refuse instead of guessing.
    let mut overreaching: Vec<(String, Vec<knives::ids::CommitId>)> = Vec::new();
    for (index, branch, _tip) in &advances {
        let Some(stale) = parents.get(*index).cloned() else {
            continue;
        };
        match overreaching.iter_mut().find(|entry| &entry.0 == branch) {
            Some(entry) => entry.1.push(stale),
            None => overreaching.push((branch.clone(), vec![stale])),
        }
    }
    overreaching.retain(|(_, stale_parents)| stale_parents.len() > 1);
    if !overreaching.is_empty() {
        for (branch, stale_parents) in &overreaching {
            let listed: Vec<String> = stale_parents.iter().map(short12).collect();
            println!(
                "{repo}: {branch} descends from {} parents of {} ({}); drop and include instead",
                stale_parents.len(),
                release.name,
                listed.join(", ")
            );
        }
        println!("{repo}: nothing advanced; one candidate cannot silently replace several members");
        return Ok(None);
    }
    let mut moved: Vec<String> = Vec::new();
    for (index, branch, tip) in advances {
        if let Some(slot) = parents.get_mut(index) {
            *slot = tip;
        }
        moved.push(branch);
    }
    Ok(Some((parents, moved)))
}

/// Advance exactly the named branches, refusing anything unanswerable: `None`
/// is a refusal that has already said why.
fn advance_named_members(
    context: &EditContext<'_>,
    carried: &[(String, knives::ids::CommitId)],
    branches: &[BranchName],
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, Vec<String>)>> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let mut parents = release.parents.clone();
    let mut moved: Vec<String> = Vec::new();
    for branch in branches {
        let named = carried
            .iter()
            .find(|(carried, _)| carried == branch.as_str())
            .map(|(_, tip)| tip.clone());
        let Some(tip) = named else {
            if bookmark_tip(opened, branch.as_str())?.is_some() {
                // The trunk and our release names are the two bookmarks a release
                // can never carry, and naming one here reached the same mover the
                // bare advance deliberately keeps them away from.
                println!(
                    "{repo}: {branch} is the trunk or a release name, so it is never a member \
                     of {}",
                    release.name
                );
            } else {
                println!("{repo}: no local bookmark named {branch}");
            }
            return Ok(None);
        };
        if parents.contains(&tip) {
            println!("{repo}: {branch} is already at its tip in {}", release.name);
            continue;
        }
        if opened.is_ancestor(&release.commit, &tip)? {
            println!(
                "{repo}: {branch} is stacked on {}, so advancing a member onto it would fold \
                 in work nobody included and put the cut in its own ancestry",
                release.name
            );
            return Ok(None);
        }
        // Matched by commit rather than by position, so no index invariant has to
        // hold and the count and the listing below cannot disagree.
        let mut matched = Vec::new();
        for parent in &parents {
            if parent != &tip && opened.is_ancestor(parent, &tip)? {
                matched.push(parent.clone());
            }
        }
        match matched.as_slice() {
            [] => {
                println!(
                    "{repo}: {} carries no parent of {branch}; `knives release include \
                     {branch}` adds it",
                    release.name
                );
                return Ok(None);
            }
            [parent] => {
                for slot in parents.iter_mut().filter(|slot| **slot == *parent) {
                    *slot = tip.clone();
                }
                moved.push(branch.to_string());
            }
            many => {
                let listed: Vec<String> = many.iter().map(short12).collect();
                println!(
                    "{repo}: {branch} descends from {} parents of {} ({}); drop and \
                     include instead",
                    many.len(),
                    release.name,
                    listed.join(", ")
                );
                return Ok(None);
            }
        }
    }
    Ok(Some((parents, moved)))
}

/// Advance one named branch onto the parent explicitly given by `old`,
/// bypassing the ancestry search entirely.
///
/// Ancestry is the right default: it survives an ordinary `jj rebase`, which
/// keeps a commit's descendants reachable from it. It cannot survive a `jj
/// duplicate` rebuild -- routine in these repos precisely because `jj rebase
/// -s` drags in whatever else is stacked on the branch -- which produces a new
/// change id sharing no ancestry with the commit it replaces. Without this,
/// the only way forward is `drop` then `include`, which loses the release's
/// recorded resolution for that member and rebuilds it from scratch.
fn advance_named_member_from(
    context: &EditContext<'_>,
    carried: &[(String, knives::ids::CommitId)],
    branch: &BranchName,
    old: &knives::ids::CommitId,
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, Vec<String>)>> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let Some(tip) = carried
        .iter()
        .find(|(carried, _)| carried == branch.as_str())
        .map(|(_, tip)| tip.clone())
    else {
        if bookmark_tip(opened, branch.as_str())?.is_some() {
            println!(
                "{repo}: {branch} is the trunk or a release name, so it is never a member \
                 of {}",
                release.name
            );
        } else {
            println!("{repo}: no local bookmark named {branch}");
        }
        return Ok(None);
    };
    if opened.is_ancestor(&release.commit, &tip)? {
        println!(
            "{repo}: {branch} is stacked on {}, so advancing a member onto it would fold in \
             work nobody included and put the cut in its own ancestry",
            release.name
        );
        return Ok(None);
    }
    let mut parents = release.parents.clone();
    if parents.contains(&tip) {
        println!("{repo}: {branch} is already at its tip in {}", release.name);
        return Ok(Some((parents, Vec::new())));
    }
    let Some(index) = parents.iter().position(|parent| parent == old) else {
        println!(
            "{repo}: {} is not a parent of {}; `knives release include {branch}` adds \
             {branch} instead",
            short12(old),
            release.name
        );
        return Ok(None);
    };
    if let Some(slot) = parents.get_mut(index) {
        *slot = tip;
    }
    Ok(Some((parents, vec![branch.to_string()])))
}

/// A local bookmark's tip, when the name is one.
fn bookmark_tip(
    opened: &knives::jj::Repo,
    name: &str,
) -> anyhow::Result<Option<knives::ids::CommitId>> {
    Ok(opened
        .bookmark_tips()?
        .get(&knives::ids::BookmarkRef::Local(
            knives::ids::BranchName::new(name),
        ))
        .cloned())
}

/// Name each parent for the release description: the branch holding it, the
/// trunk it descends from, or its own id when nothing else does.
fn parent_sources(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    scheme: &ReleaseScheme,
    parents: &[knives::ids::CommitId],
) -> anyhow::Result<Vec<(String, knives::ids::CommitId)>> {
    let tips = opened.bookmark_tips()?;
    let carried = release::carried_from_tips(&tips, entry.trunk(), scheme);
    let trunk_tip = opened.resolve_commit(&entry.upstream_trunk()).ok();
    let mut sources = Vec::new();
    for commit in parents {
        let named = carried
            .iter()
            .find(|(_, tip)| tip == commit)
            .map(|(branch, _)| branch.clone());
        let source = if let Some(named) = named {
            named
        } else if let Some(trunk) = &trunk_tip
            && opened.is_ancestor(commit, trunk)?
        {
            entry.upstream_trunk()
        } else {
            short12(commit)
        };
        sources.push((source, commit.clone()));
    }
    Ok(sources)
}

/// A commit id at the length this program shows them: enough to be unique in a
/// fork, short enough to read in a line of prose.
fn short12(commit: &knives::ids::CommitId) -> String {
    commit.as_str().chars().take(12).collect()
}

/// The ledger writer for a command acting on `entry`.
///
/// The owner is resolved exactly as a claim's is, so one agent's events and its
/// claims carry the same name and a reader can join them.
fn scribe_for(repo: &RepoName, entry: &knives::config::RepoEntry) -> anyhow::Result<Scribe> {
    let owner = knives::commands::claim::current_owner(&std::env::current_dir()?)?;
    Ok(Scribe::new(
        Ledger::for_repo(repo),
        repo.clone(),
        entry.path.clone(),
        owner,
    ))
}

/// What a `finish` did, or nothing when it did nothing.
///
/// Releasing a claim and recording a supersession are two acts and either can
/// happen alone: a `finish` on an unheld branch releases no claim, and one with
/// `--superseded-by` still records where the work went.
fn release_event(had: bool, superseded_by: Option<&str>) -> Option<String> {
    match (had, superseded_by) {
        (true, Some(replacement)) => Some(format!("claim released; superseded by {replacement}")),
        (true, None) => Some("claim released".to_owned()),
        (false, Some(replacement)) => Some(format!("superseded by {replacement}")),
        (false, None) => None,
    }
}

/// The open pull request a branch still owns, if the forge proves it has one.
///
/// A stated number is authoritative when it is open; otherwise the branch's
/// currently primary pull request is considered too. Every number surfaced for
/// that guard needs a same-run fact; an omitted fact cannot prove it is closed.
fn open_pull_for(
    target: &BranchTarget,
    entry: &knives::config::RepoEntry,
    store: &Store,
    cache_root: Option<&std::path::Path>,
) -> Result<Option<knives::forge::PullRequest>, knives::forge::ForgeError> {
    let remotes = [
        entry.remote(knives::config::Role::Origin),
        entry.remote(knives::config::Role::Release),
    ];
    let forge = CliForge;
    let opened = knives::snapshot::open(knives::snapshot::SnapshotConfig {
        forge: &forge,
        path: &entry.path,
        remotes,
        cache_root,
    })?;
    let discovery = opened.discover()?;
    let stated = store.tracked_pull(target);
    let discovery_primary = knives::forge::index_pulls(&discovery.ours())
        .by_branch
        .get(&target.branch)
        .map(|pull| pull.number);
    let mut surfaced = std::collections::BTreeSet::new();
    for number in [stated, discovery_primary].into_iter().flatten() {
        let _ = surfaced.insert(number);
    }
    let numbers: Vec<u64> = surfaced.iter().copied().collect();
    let snapshot = discovery.complete(&numbers)?;
    let primary = knives::forge::index_pulls(&snapshot.ours())
        .by_branch
        .get(&target.branch)
        .map(|pull| pull.number);
    if let Some(number) = primary {
        let _ = surfaced.insert(number);
    }
    let unanswered: Vec<u64> = surfaced
        .iter()
        .copied()
        .filter(|number| snapshot.fact(*number).is_none())
        .collect();
    let result = (|| {
        if !unanswered.is_empty() {
            return Err(knives::forge::ForgeError::Query {
                detail: format!(
                    "the forge did not report facts for requested pull request(s) {}",
                    numbered(&unanswered)
                ),
            });
        }
        let mut open = None;
        for number in [stated, primary].into_iter().flatten() {
            let Some(fact) = snapshot.fact(number) else {
                return Err(knives::forge::ForgeError::Query {
                    detail: format!(
                        "the forge did not report facts for requested pull request #{number}"
                    ),
                });
            };
            if fact.pull.is_open() {
                open = Some(fact.pull.clone());
                break;
            }
        }
        Ok(open)
    })();
    let _ = snapshot.persist(None);
    result
}

/// Hand a branch back and remove its workspace. The inverse of `start`.
///
/// Removing the directory loses no work: jj snapshots a working copy into a commit, so
/// every change made there is already in the repository and reachable by change id. What
/// does not survive is anything jj never tracked, which is what `--no-cleanup` is for.
fn run_finish(
    target: &BranchTarget,
    superseded_by: Option<&str>,
    cleanup: bool,
    allow_open: bool,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(&target.repo) else {
        eprintln!("unknown repo {}", target.repo);
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    let cache_root = knives::forge_cache::cache_root();
    if !allow_open {
        match open_pull_for(target, entry, &store, cache_root.as_deref()) {
            Ok(Some(pull)) => {
                println!(
                    "{}: {} is the head of open pull request #{} ({}); merge or close it first, \
                     or pass --allow-open",
                    target.repo, target.branch, pull.number, pull.url
                );
                return Ok(Exit::Findings);
            }
            Ok(None) => {}
            Err(error) => {
                println!(
                    "{}: cannot verify whether {} has an open pull request ({error}); \
                     fix the forge login or pass --allow-open",
                    target.repo, target.branch
                );
                return Ok(Exit::Incomplete);
            }
        }
    }
    let had = store.release_claim(target);
    if let Some(new) = superseded_by {
        store.supersede(target, new);
    }
    let pr = store.tracked_pull(target);
    store.save()?;
    // What happened, and nothing else. This command runs happily on a branch
    // nobody held — it says "was not held" and forgets the workspace anyway —
    // and an event asserting a release would be a false fact in the one record
    // that exists to be believed later.
    if let Some(text) = release_event(had, superseded_by) {
        scribe_for(&target.repo, entry)?.event(Some(target.branch.as_str()), text, pr)?;
    }

    let claim = if had { "released" } else { "was not held" };
    let workspace = knives::commands::wip::workspace_for(target.branch.as_str());
    if let Err(error) = knives::jj::forget_workspace(&entry.path, &workspace) {
        println!("{target}: claim {claim}; no workspace forgotten ({error})");
        return Ok(Exit::Ok);
    }
    let directory = entry.path.parent().map(|parent| parent.join(&workspace));
    match (cleanup, directory) {
        (true, Some(directory)) if directory.is_dir() => {
            // Safe because jj already snapshotted the working copy into a commit: the
            // work is in the repository and reachable by change id. Untracked files are
            // the exception, which is what --no-cleanup is for.
            std::fs::remove_dir_all(&directory)?;
            println!(
                "{target}: claim {claim}, workspace {workspace} removed ({}); its commits \
                 remain in the repository",
                directory.display()
            );
        }
        (_, directory) => println!(
            "{target}: claim {claim}, workspace {workspace} forgotten; {} left on disk",
            directory.map_or_else(|| "its directory".to_owned(), |d| d.display().to_string())
        ),
    }
    Ok(Exit::Ok)
}

/// State or forget which pull request a branch belongs to.
fn run_track(
    target: &BranchTarget,
    pr: Option<u64>,
    fork_only: bool,
    forget: bool,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(&target.repo) else {
        eprintln!("unknown repo {}", target.repo);
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    // Read before the change, so a withdrawal is still filed under the number it
    // withdrew.
    let stated = store.tracked_pull(target);
    // Each branch stamps the number its entry is ABOUT, not whatever happened to
    // be stated a moment earlier. The event that creates an association is the
    // one `knives notch --pr <n>` most needs to find, and stamping the prior
    // value there — usually nothing — would hide it from the only filter the
    // field exists for.
    let (text, stamped) = if fork_only {
        store.mark_fork_only(target, "stated with `knives track --fork-only`");
        (
            "stated as having no upstream pull request".to_owned(),
            stated,
        )
    } else if forget {
        let had = store.untrack_pull(target);
        (
            if had {
                "pull request statement forgotten".to_owned()
            } else {
                "no pull request statement to forget".to_owned()
            },
            stated,
        )
    } else {
        let Some(number) = pr else {
            eprintln!("give --pr <number>, or --forget");
            return Ok(Exit::Usage);
        };
        store.track_pull(target, number);
        (format!("stated as #{number}"), Some(number))
    };
    store.save()?;
    scribe_for(&target.repo, entry)?.event(Some(target.branch.as_str()), text.clone(), stamped)?;
    println!("{target} {}", spoken(&text));
    Ok(Exit::Ok)
}

/// The prose form of a `track` outcome, which reads about the branch rather than
/// about the statement.
fn spoken(text: &str) -> String {
    match text {
        "stated as having no upstream pull request" => {
            "deliberately has no upstream pull request".to_owned()
        }
        "pull request statement forgotten" => "is back to inferring its pull request".to_owned(),
        "no pull request statement to forget" => "had no stated pull request".to_owned(),
        stated => stated.replacen("stated as ", "is ", 1),
    }
}

/// Record what a branch cannot land before.
///
/// Requirements are validated against the registry, because a dependency on a repo
/// knives does not manage is a typo, and a typo that records silently is worse than
/// no dependency at all: it reads as satisfied forever.
fn run_depends(target: &BranchTarget, on: &[String]) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let mut requirements = Vec::new();
    for text in on {
        let Some(requirement) = Requirement::parse(text) else {
            eprintln!("cannot read {text} as a requirement; write it as `<repo>#<number>`");
            return Ok(Exit::Usage);
        };
        if registry.get(&requirement.repo).is_none() {
            let known: Vec<String> = registry.names().map(|n| n.to_string()).collect();
            eprintln!(
                "unknown repo {} in {text}; known: {}",
                requirement.repo,
                known.join(", ")
            );
            return Ok(Exit::Usage);
        }
        requirements.push(requirement);
    }
    // Resolved before anything is written. Dispatch already validated this name
    // through `one_repo`, so an absent entry is an invariant violation rather
    // than a user error — and the one thing not to do with it is mutate the
    // store and then quietly skip the ledger, which would leave a dependency
    // recorded and unexplained.
    let Some(entry) = registry.get(&target.repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!("unknown repo {}; known: {}", target.repo, known.join(", "));
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    store.add_dependencies(target, &requirements);
    let pr = store.tracked_pull(target);
    store.save()?;
    let listed: Vec<String> = requirements.iter().map(ToString::to_string).collect();
    scribe_for(&target.repo, entry)?.event(
        Some(target.branch.as_str()),
        format!("requires {}", listed.join(", ")),
        pr,
    )?;
    println!("{target} now requires {}", listed.join(", "));
    Ok(Exit::Ok)
}

/// Repos a reporting command covers.
///
/// A name wins. Otherwise the repo you are standing in, because that is nearly always
/// what you meant, and reporting on ten repositories at once is how `status` became
/// unreadable. `--all` asks for all of them explicitly, and standing outside every
/// managed repo also means all of them, since there is nothing else it could mean.
fn selected(
    requested: Option<&str>,
    all: bool,
) -> anyhow::Result<Result<Vec<(RepoName, knives::config::RepoEntry)>, Exit>> {
    let registry = load(&default_config_path())?;
    if registry.is_empty() {
        eprintln!(
            "no repos configured; add entries to {}",
            default_config_path().display()
        );
        return Ok(Err(Exit::Usage));
    }
    let every = || -> Vec<(RepoName, knives::config::RepoEntry)> {
        registry
            .repos
            .iter()
            .map(|(name, entry)| (RepoName::new(name.clone()), entry.clone()))
            .collect()
    };
    if let Some(name) = requested {
        return Ok(registry.get(&RepoName::new(name)).map_or_else(
            || {
                let known: Vec<String> = registry.names().map(|n| n.to_string()).collect();
                eprintln!("unknown repo {name}; known: {}", known.join(", "));
                Err(Exit::Usage)
            },
            |entry| Ok(vec![(RepoName::new(name), entry.clone())]),
        ));
    }
    if all {
        return Ok(Ok(every()));
    }
    let here = std::env::current_dir()?;
    Ok(Ok(registry
        .containing(&here)
        .map_or_else(every, |(name, entry)| {
            vec![(name, entry.clone())]
        })))
}

/// Which repos a report covers.
#[derive(Debug, Clone, Copy)]
struct Scope {
    all: bool,
}

/// How a report is produced and shown.
#[derive(Debug, Clone, Copy)]
struct StatusView {
    scope: Scope,
    /// What the report gathers, as opposed to how it is displayed.
    gather: Gather,
    display: Display,
}

#[derive(Debug, Clone, Copy)]
struct Gather {
    probe: bool,
    use_forge: bool,
}

#[derive(Debug, Clone, Copy)]
struct Display {
    verbose: bool,
    output: knives::cli::Output,
}

/// How many threads to run at once, from the machine's own answer.
fn parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}
/// Headroom under the forge's documented 100-concurrent-request cap.
const MAX_REPO_WORKERS: usize = 64;

fn worker_budget(repositories: usize, parallelism: usize) -> (usize, usize) {
    let repo_workers = repositories.clamp(1, parallelism.min(MAX_REPO_WORKERS));
    let probe_workers = (parallelism / repo_workers).max(1);
    (repo_workers, probe_workers)
}

fn run_status(requested: Option<&str>, view: StatusView) -> anyhow::Result<Exit> {
    let StatusView {
        scope: Scope { all },
        gather: Gather { probe, use_forge },
        display: Display { verbose, output },
    } = view;
    let chosen = match selected(requested, all)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let store = Store::open(default_state_path())?;
    let registry = load(&default_config_path())?;
    let cli_forge = CliForge;
    let forge: Option<&dyn Forge> = if use_forge { Some(&cli_forge) } else { None };
    let cache_root = knives::forge_cache::cache_root();
    let cache = cache_root.as_deref();

    // Bounded on both axes, because they multiply: repositories are chunked
    // across at most `repo_workers` threads, and each of those divides the
    // machine's parallelism among its probes. Spawning one thread per repository
    // instead would put a ten-repo registry's probe threads at ten times the
    // budget, and this work is index reads and repository handles, not idle
    // waiting. Chunked rather than queued for the same reason the probes are: the
    // bound is the point and a queue would be a dependency.
    let (repo_workers, probe_workers) = worker_budget(chosen.len(), parallelism());
    let chunk = chosen.len().div_ceil(repo_workers).max(1);
    let store = &store;
    let registry = &registry;
    let gathered: Vec<anyhow::Result<(RepoName, status::Report, status::Timings)>> =
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(repo_workers);
            for slice in chosen.chunks(chunk) {
                handles.push((
                    slice,
                    scope.spawn(move || {
                        slice
                            .iter()
                            .map(|(name, entry)| {
                                let ledger = Ledger::for_repo(name);
                                let (report, timings) = status::gather_timed(
                                    name,
                                    entry,
                                    store,
                                    &status::Options {
                                        probe,
                                        forge,
                                        cache,
                                        registry: Some(registry),
                                        ledger: Some(&ledger),
                                        workers: probe_workers,
                                    },
                                )?;
                                Ok((name.clone(), report, timings))
                            })
                            .collect::<Vec<_>>()
                    }),
                ));
            }
            handles
                .into_iter()
                .flat_map(|(slice, handle)| {
                    handle.join().unwrap_or_else(|_| {
                        slice
                            .iter()
                            .map(|(name, _)| Err(anyhow::anyhow!("gathering {name} panicked")))
                            .collect()
                    })
                })
                .collect()
        });

    let mut worst = Exit::Ok;
    let mut first = true;
    for gathered in gathered {
        let (name, report, timings) = gathered?;
        if let Some(payload) = knives::cli::machine_payload(output, &report)? {
            println!("{payload}");
        } else {
            if !first {
                println!();
            }
            first = false;
            println!("{}", status::render(&report, verbose));
        }
        // stderr, so a timed run's stdout is still the report a script parses.
        if knives::timing::enabled() {
            eprintln!("{}", timings.line(name.as_str()));
        }
        worst = worst.worst(status::exit_for(&report));
    }
    Ok(worst)
}

enum ReleaseInvocation {
    Plan,
    Cut {
        name: Option<String>,
        allow_drop: bool,
    },
}

fn run_release(
    name: &str,
    extra_consumers: &[&std::path::Path],
    invocation: &ReleaseInvocation,
) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut worst = Exit::Ok;
    for (repo, entry) in chosen {
        // Consumers recorded in the registry are the answer to `--consumer`; asking for
        // them again every time is how the flag stayed unexplained and unused. A flag,
        // when given, adds to them rather than replacing: a fork has however many
        // consumers it has, and asking about one more does not unrecord the others.
        let mut consumers = entry.consumers.clone();
        consumers.extend(extra_consumers.iter().map(|path| path.to_path_buf()));
        let opened = knives::jj::Repo::open(&entry.path)?;
        let scheme = entry.release_scheme();
        let (cut_name, allow_drop) = match requested_cut(invocation, &scheme) {
            Ok(request) => request,
            Err(exit) => return Ok(exit),
        };
        worst = worst.worst(release_plan_exit(&repo, &entry, &consumers, &opened)?);

        if let Some(name) = cut_name {
            let trunk_name = entry.upstream_trunk();
            let trunk = opened.resolve_commit(&trunk_name)?;
            if let Some(orphaned) = check_orphan_commits_before_cut(&opened, &entry, trunk.clone())?
                && let Some(exit) = report_orphaned_cut(&repo, &orphaned, allow_drop)
            {
                return Ok(exit);
            }
            let tips = opened.bookmark_tips()?;
            let previous = release::previous_release_for_cut(&entry, &tips);
            let previous_commit = previous.as_ref().map(|(_, commit)| commit.clone());
            // A cut is a new name for the composition in hand, never a recomputation:
            // with a previous release its parents are carried verbatim — nothing joins,
            // nothing advances, and a branch enters through `release include`. Only the
            // first cut has no composition to carry, so it starts from every branch: a
            // release is a flat merge of feature and fix branches, and the upstream
            // base is never a direct parent — it is reachable through every member.
            let (carried, members, audit_base) = if let Some((_, previous)) = &previous {
                let parents: Vec<knives::ids::CommitId> = opened
                    .parents_of(previous.as_str())?
                    .into_iter()
                    .map(|parent| parent.commit)
                    .collect();
                let carried = parent_sources(&opened, &entry, &scheme, &parents)?;
                let members = carried.clone();
                let base = release::shared_base(&opened, previous, &trunk)?
                    .unwrap_or_else(|| trunk.clone());
                (carried, members, base)
            } else {
                let carried = release::carried_branches(&opened, entry.trunk(), &scheme)?;
                if carried.is_empty() {
                    println!(
                        "{repo}: no branches to cut; a release is a flat merge of feature \
                         and fix branches, and there are none"
                    );
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
            let request = cut_request(name.clone(), &carried);
            let mut candidate =
                release::candidate_cut(&entry.path, &request, previous_commit.as_ref())?;
            // An audit error or failure simply DROPS the candidate: the merge
            // was never a published operation, so there is nothing to abandon
            // and no crash window that strands one.
            let audit = release::audit_cut(
                &entry.path,
                &members,
                release::CutSubject::Candidate(&mut candidate),
                release::AuditContext {
                    previous: previous_commit.as_ref(),
                    trunk: &audit_base,
                },
            )?;
            if let Some(exit) = report_cut_audit(&repo, &audit) {
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
                match recorded_composition_check(&repo, &mut candidate, &gate, allow_drop)? {
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
            record_cut_event(&repo, &entry, &completed)?;
            worst = worst.worst(report_completed_cut(&repo, &entry, &opened, &completed)?);
        }
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

fn release_plan_exit(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    consumers: &[std::path::PathBuf],
    opened: &knives::jj::Repo,
) -> anyhow::Result<Exit> {
    let plan = release::plan(repo, entry, consumers)?;
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
) -> anyhow::Result<Result<(Option<release::RecordedCut>, release::CompositionCheck), Exit>> {
    let recorded = release::last_recorded_cut(&Ledger::for_repo(repo).entries()?);
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
    recorded: Option<&release::RecordedCut>,
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
    recorded: Option<&'a release::RecordedCut>,
    check: &'a release::CompositionCheck,
}

/// Record which branches and commits became a published release cut.
fn record_cut_event(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    cut: &CompletedCut<'_>,
) -> anyhow::Result<()> {
    let opened = knives::jj::Repo::open(&entry.path)?;
    let parents: Vec<knives::ids::CommitId> = opened
        .parents_of(cut.created.as_str())?
        .into_iter()
        .map(|parent| parent.commit)
        .collect();
    let members = parent_sources(&opened, entry, cut.scheme, &parents)?;
    let members_text = members
        .iter()
        .map(|(source, commit)| format!("{source}@{}", short12(commit)))
        .collect::<Vec<_>>()
        .join(", ");
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
    scribe_for(repo, entry)?.record(&Draft {
        subject: Some(cut.name),
        kind: Kind::Event,
        text: format!(
            "cut {} as {} with {} parent(s): {members_text}{delta}",
            cut.name,
            short12(cut.created),
            members.len()
        ),
        evidence,
        pr: None,
    })?;
    Ok(())
}

fn report_completed_cut(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    opened: &knives::jj::Repo,
    cut: &CompletedCut<'_>,
) -> anyhow::Result<Exit> {
    let mut post_cut_exit =
        if cut.audit.inconclusive.is_empty() && cut.check.inconclusive.is_empty() {
            Exit::Ok
        } else {
            Exit::Findings
        };
    print_previous_release_position(opened, entry);
    println!(
        "  cut {} as {} with {} parent(s), flat, not pushed",
        cut.name,
        short12(cut.created),
        cut.request.parents.len()
    );
    match knives::jj::conflicted_files(&entry.path, cut.created.as_str()) {
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    if let Some(first) = cut.request.parents.first() {
        let test_count = release::check_test_count(&entry.path, entry, cut.created, first);
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
        release::carried_branches(opened, entry.trunk(), &entry.release_scheme())?
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
    Ok(post_cut_exit.worst(reap_after_cut(repo, entry)?))
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
        println!("    {}", short12(commit));
    }
    println!("  re-run with --allow-drop to state this is intended");
    Some(Exit::Incomplete)
}

fn check_orphan_commits_before_cut(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    trunk: knives::ids::CommitId,
) -> anyhow::Result<Option<OrphanedLineage>> {
    let scheme = entry.release_scheme();
    let tips = opened.bookmark_tips()?;
    let Some(previous) = release::previous_release_for_cut(entry, &tips) else {
        return Ok(None);
    };
    let mut keep: Vec<knives::ids::CommitId> = tips
        .iter()
        .filter_map(|(reference, commit)| match reference {
            knives::ids::BookmarkRef::Local(branch)
                if !knives::ids::is_release_name(branch, &scheme) =>
            {
                Some(commit.clone())
            }
            _ => None,
        })
        .collect();
    keep.push(trunk);
    let orphans = release::orphaned_commits(&entry.path, &previous.1, &keep, &tips)?;
    if orphans.is_empty() {
        return Ok(None);
    }
    Ok(Some(OrphanedLineage {
        previous: previous.0,
        commits: orphans,
    }))
}

fn cut_request(name: String, carried: &[(String, knives::ids::CommitId)]) -> release::Cut {
    release::Cut {
        name,
        parents: carried.iter().map(|(_, commit)| commit.clone()).collect(),
        provenance: carried
            .iter()
            .map(|(branch, commit)| (commit.clone(), branch.clone()))
            .collect(),
    }
}

/// Reap superseded dated cuts now that a newer one exists.
///
/// Under `Fixed` the enumeration is empty by construction (no dated names), so
/// this is a no-op there. Opens the repository again deliberately: the caller's
/// handle predates the cut and reads stale tips, under which the superseded cut
/// is still the newest dated name and nothing is reaped at all.
fn reap_after_cut(repo: &RepoName, entry: &knives::config::RepoEntry) -> anyhow::Result<Exit> {
    let reopened = knives::jj::Repo::open(&entry.path)?;
    let report = release::reap_superseded(&entry.path, &reopened)?;
    print_reap(&repo.to_string(), &report);
    Ok(reap_exit(&report))
}

/// Reap superseded dated cuts on demand.
fn run_reap(name: &str) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut worst = Exit::Ok;
    for (repo, entry) in chosen {
        let opened = knives::jj::Repo::open(&entry.path)?;
        let report = release::reap_superseded(&entry.path, &opened)?;
        print_reap(&repo.to_string(), &report);
        worst = worst.worst(reap_exit(&report));
    }
    Ok(worst)
}

/// Return a finding when reaping leaves work or reports an incomplete cleanup.
///
/// `reap_superseded` records every `forgotten_only` entry with a corresponding
/// note, so `notes` covers that state without a redundant condition here.
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
    for name in &report.forgotten_only {
        println!("{repo}: {name}: refs forgotten; commit abandon refused (see note)");
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
            short12(&commit)
        );
    } else if matches!(entry.release_scheme(), ReleaseScheme::Fixed(_)) {
        println!("  no previous release position: this is the first cut of the fixed branch");
    }
}

fn run_preflight(name: &str) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut store = Store::open_for_update(default_state_path())?;
    let forge = CliForge;
    let mut worst = Exit::Ok;
    let cache_root = knives::forge_cache::cache_root();
    for (repo, entry) in chosen {
        let report = preflight::gather(preflight::GatherInput {
            name: &repo,
            entry: &entry,
            store: &mut store,
            forge: &forge,
            cache: cache_root.as_deref(),
        });
        println!("{}", preflight::render(&report));
        worst = worst.worst(preflight::exit_for(&report));
    }
    store.save()?;
    Ok(worst)
}

fn run_sync(
    requested: Option<&str>,
    all: bool,
    output: knives::cli::Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let here = std::env::current_dir()?;
    let chosen = match sync_targets(&registry, requested, all, &here) {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut store = Store::open_for_update(default_state_path())?;
    let cli_forge = CliForge;
    let forge = use_forge.then_some(&cli_forge as &dyn Forge);
    let cache_root = knives::forge_cache::cache_root();

    let mut worst = Exit::Ok;
    for (name, entry) in chosen {
        let scribe = scribe_for(&name, &entry)?;
        let report = sync::sync_repo(sync::SyncInput {
            entry: &entry,
            store: &mut store,
            forge,
            scribe: &scribe,
            cache: cache_root.as_deref(),
        })?;
        if let Some(payload) = knives::cli::machine_payload(output, &report)? {
            println!("{payload}");
        } else {
            println!("{}", sync::render(&report));
        }
        worst = worst.worst(sync::exit_for(&report));
    }
    Ok(worst)
}

fn sync_targets(
    registry: &knives::config::Registry,
    requested: Option<&str>,
    all: bool,
    cwd: &std::path::Path,
) -> Result<Vec<(RepoName, knives::config::RepoEntry)>, Exit> {
    if let Some(name) = requested {
        registry.get(&RepoName::new(name)).map_or_else(
            || {
                let known: Vec<String> = registry.names().map(|n| n.to_string()).collect();
                eprintln!("unknown repo {name}; known: {}", known.join(", "));
                Err(Exit::Usage)
            },
            |entry| Ok(vec![(RepoName::new(name), entry.clone())]),
        )
    } else if all {
        if registry.is_empty() {
            eprintln!(
                "no repos configured; add entries to {}",
                default_config_path().display()
            );
            return Err(Exit::Usage);
        }
        Ok(registry
            .repos
            .iter()
            .map(|(name, entry)| (RepoName::new(name.clone()), entry.clone()))
            .collect())
    } else if let Some((name, entry)) = registry.containing(cwd) {
        Ok(vec![(name, entry.clone())])
    } else {
        eprintln!("give a repo name, or --all");
        Err(Exit::Usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knives::config::{Registry, RepoEntry, TrustRules};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn worker_budget_caps_large_repository_sets_under_the_forge_limit() {
        let (repo_workers, probe_workers) = worker_budget(101, 101);

        assert_eq!(repo_workers, MAX_REPO_WORKERS);
        assert!(repo_workers <= MAX_REPO_WORKERS);
        assert!(probe_workers >= 1);
    }

    #[test]
    fn sync_targets_bare_in_managed_repo_selects_that_repo() {
        let mut repos = BTreeMap::new();
        repos.insert(
            "scout".to_string(),
            RepoEntry {
                path: PathBuf::from("/path/to/scout"),
                upstream: "https://example.test/org/scout".to_string(),
                origin: "https://example.test/ours/scout".to_string(),
                base: None,
                release: None,
                release_branch: None,
                test_count_command: None,
                consumers: vec![],
            },
        );
        let registry = Registry {
            repos,
            trusted: BTreeMap::new(),
            trust: TrustRules::default(),
        };

        let cwd = PathBuf::from("/path/to/scout/subdirectory");
        let result = sync_targets(&registry, None, false, &cwd);
        assert!(result.is_ok());
        let selected = result.unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected.first().unwrap().0.as_str(), "scout");
    }

    #[test]
    fn track_prose_preserves_the_established_human_output() {
        for (event, expected) in [
            ("stated as #4545", "is #4545"),
            (
                "stated as having no upstream pull request",
                "deliberately has no upstream pull request",
            ),
            (
                "pull request statement forgotten",
                "is back to inferring its pull request",
            ),
            (
                "no pull request statement to forget",
                "had no stated pull request",
            ),
        ] {
            assert_eq!(spoken(event), expected);
        }
    }

    #[test]
    fn sync_targets_bare_outside_managed_repo_returns_usage() {
        let mut repos = BTreeMap::new();
        repos.insert(
            "scout".to_string(),
            RepoEntry {
                path: PathBuf::from("/path/to/scout"),
                upstream: "https://example.test/org/scout".to_string(),
                origin: "https://example.test/ours/scout".to_string(),
                base: None,
                release: None,
                release_branch: None,
                test_count_command: None,
                consumers: vec![],
            },
        );
        let registry = Registry {
            repos,
            trusted: BTreeMap::new(),
            trust: TrustRules::default(),
        };

        let cwd = PathBuf::from("/path/to/other");
        let result = sync_targets(&registry, None, false, &cwd);
        assert_eq!(result, Err(Exit::Usage));
    }
}
