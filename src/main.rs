//! The `knives` binary.
//!
//! Dispatch only. Every command owns its own logic and returns an [`Exit`], so
//! this file never grows a decision.
// allow: SIZE_OK: 1070 lines - dispatch-only, splitting would scatter the exhaustive match.

use std::process::ExitCode;

use clap::Parser as _;
use knives::cli::{Cli, Command, Exit, ReleaseAction};
use knives::commands::{hook, init, preflight, register, release, repos, start, status, sync};
use knives::config::{default_config_path, load};
use knives::forge::{CliForge, Forge};
use knives::ids::{BranchName, BranchTarget, ReleaseScheme, RepoName, Requirement};
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

fn dispatch() -> anyhow::Result<Exit> {
    let cli = Cli::parse();
    let json = knives::cli::machine_readable(cli.json, cli.text);
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
                display: Display { verbose, json },
            },
        ),
        Command::Sync {
            repo,
            all,
            no_github,
        } => run_sync(repo.as_deref(), all, json, !no_github),
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
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_finish(
                &BranchTarget::new(name, BranchName::new(branch)),
                superseded_by.as_deref(),
                !no_cleanup,
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
            dispatch_release(chosen, action, &extra)
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
    chosen: RepoName,
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
        Some(ReleaseAction::Rebase { reference }) => {
            run_rebase(chosen.as_str(), reference.as_deref())
        }
        Some(ReleaseAction::Reap) => run_reap(chosen.as_str()),
        Some(ReleaseAction::Include { branch, why }) => run_membership(
            &BranchTarget::new(chosen, BranchName::new(branch)),
            Membership::In(why),
        ),
        Some(ReleaseAction::Drop { branch, why }) => run_membership(
            &BranchTarget::new(chosen, BranchName::new(branch)),
            Membership::Out(why),
        ),
    }
}

/// Replace superseded release bases with an upstream commit, keeping branch parents.
///
/// A cut deliberately does not do this: which upstream commit a release should contain,
/// and whether to move it at all, is a judgment. What this exists for is the case where a
/// pull request has merged upstream — until the release contains the commit that merge
/// landed in, dropping the local branch removes the change from the release too.
fn run_rebase(name: &str, reference: Option<&str>) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut worst = Exit::Ok;
    for (repo, entry) in chosen {
        let reference = reference.map_or_else(|| entry.upstream_trunk(), str::to_owned);
        let opened = knives::jj::Repo::open(&entry.path)?;
        let plan = release::plan(&repo, &entry, &entry.consumers)?;
        let Some(release_name) = plan.release.clone() else {
            println!("{repo}: no release to move");
            continue;
        };
        let onto = opened.resolve_commit(&reference)?;
        let release_commit = opened.resolve_commit(&release_name)?;
        // Ancestry, not parent identity: a commit already reachable through a
        // parent's history is contained, and adding it again grows the octopus.
        if opened.is_ancestor(&onto, &release_commit)? {
            println!("{repo}: {release_name} already contains {reference}");
            continue;
        }
        // Follows from who pins it, rather than from an opinion: a consumer that follows
        // the branch sees a repair in place, one frozen on the revision does not.
        let scheme = entry.release_scheme();
        if release::repair_effect(&plan.pins) == release::RepairEffect::NewDatedName {
            match &scheme {
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
            worst = worst.worst(Exit::Incomplete);
            continue;
        }
        let parents = opened.parents_of(&release_name)?;
        let tips = opened.bookmark_tips()?;
        let mut carried: Vec<knives::ids::CommitId> = Vec::new();
        let mut replaced = 0usize;
        for parent in &parents {
            // Oracle amendment: a parent HELD by a live branch bookmark is kept
            // even when onto already reaches it — a landed branch remains a
            // member with its parent and provenance intact (spec 1.7 "keeping
            // branch parents"; dropping members is `release drop`'s job, never
            // the rebase's). Held = any bookmark still pointing at the parent
            // whose branch is neither a release name nor the trunk.
            let held = parent.bookmarks.iter().any(|reference| {
                tips.get(reference) == Some(&parent.commit)
                    && !knives::ids::is_release_name(reference.branch(), &scheme)
                    && reference.branch().as_str() != entry.trunk()
            });
            if held {
                carried.push(parent.commit.clone());
                continue;
            }
            // Ancestry, not parent identity: an unheld parent reachable through
            // the replacement's history is a superseded base, even when it is
            // not a direct parent of that replacement.
            if opened.is_ancestor(&parent.commit, &onto)? {
                replaced += 1;
                continue;
            }

            let no_bookmark = if parent.bookmarks.is_empty() {
                "; no bookmark points at it"
            } else {
                ""
            };
            let moved = stale_parent_moved_branches(&entry, &scheme, &parent.commit)?;
            let moved = moved.map_or_else(String::new, |moved| format!("; moved tip(s): {moved}"));
            eprintln!(
                "{repo}: refusing to rebase {release_name}: parent {} is stale{no_bookmark}{moved}. \
                 Fix the branch or drop it from the release, then re-run; carrying it could ship \
                 pre-rewrite code.",
                parent.commit.as_str().chars().take(12).collect::<String>(),
            );
            return Ok(Exit::Incomplete);
        }
        carried.push(onto.clone());
        let message = format!("chore(release): {release_name} rebased onto {reference}");
        // #12: the repair is the OLD release duplicated onto the new parent set,
        // never a from-scratch merge — prior conflict resolutions carry over, so
        // a rebase surfaces only conflicts the new base itself introduces.
        let duplicated = knives::jj::duplicate_onto(&entry.path, &release_commit, &carried)?;
        let created = knives::jj::describe_commit(&entry.path, &duplicated, &message)?;
        knives::jj::set_bookmark_anywhere(&entry.path, &release_name, created.as_str())?;
        println!(
            "{repo}: {release_name} now contains {reference} ({}), {} base parent(s) replaced, \
             {} branch parent(s) kept",
            &onto.as_str()[..12.min(onto.as_str().len())],
            replaced,
            carried.len() - 1
        );
    }
    Ok(worst)
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
        .map(|(branch, tip)| {
            format!(
                "{branch} (now {})",
                tip.as_str().chars().take(12).collect::<String>()
            )
        })
        .collect();
    Ok((!moved.is_empty()).then(|| moved.join(", ")))
}

/// What was stated about a branch's place in the next release.
enum Membership {
    In(Option<String>),
    Out(Option<String>),
}

/// State, or forget, whether a branch belongs in the next release.
fn run_membership(target: &BranchTarget, membership: Membership) -> anyhow::Result<Exit> {
    let mut store = Store::open_for_update(default_state_path())?;
    match membership {
        Membership::In(why) => {
            let why = why.unwrap_or_else(|| "stated".to_owned());
            store.include_in_release(target, &why);
            store.save()?;
            println!("{target} is in the next release ({why})");
        }
        Membership::Out(why) => {
            let why = why.unwrap_or_else(|| "stated".to_owned());
            store.drop_from_release(target, &why);
            store.save()?;
            println!("{target} is out of the next release ({why})");
        }
    }
    Ok(Exit::Ok)
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
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(&target.repo) else {
        eprintln!("unknown repo {}", target.repo);
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    let had = store.release_claim(target);
    if let Some(new) = superseded_by {
        store.supersede(target, new);
    }
    store.save()?;

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
    let mut store = Store::open_for_update(default_state_path())?;
    if fork_only {
        store.mark_fork_only(target, "stated with `knives track --fork-only`");
        store.save()?;
        println!("{target} deliberately has no upstream pull request");
        return Ok(Exit::Ok);
    }
    if forget {
        let had = store.untrack_pull(target);
        store.save()?;
        println!(
            "{target} {}",
            if had {
                "is back to inferring its pull request"
            } else {
                "had no stated pull request"
            }
        );
        return Ok(Exit::Ok);
    }
    let Some(number) = pr else {
        eprintln!("give --pr <number>, or --forget");
        return Ok(Exit::Usage);
    };
    store.track_pull(target, number);
    store.save()?;
    println!("{target} is #{number}");
    Ok(Exit::Ok)
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
    let mut store = Store::open_for_update(default_state_path())?;
    store.add_dependencies(target, &requirements);
    store.save()?;
    let listed: Vec<String> = requirements.iter().map(ToString::to_string).collect();
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
    json: bool,
}

fn run_status(requested: Option<&str>, view: StatusView) -> anyhow::Result<Exit> {
    let StatusView {
        scope: Scope { all },
        gather: Gather { probe, use_forge },
        display: Display { verbose, json },
    } = view;
    let chosen = match selected(requested, all)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let store = Store::open(default_state_path())?;
    let registry = load(&default_config_path())?;
    let cli_forge = CliForge;
    let forge: Option<&dyn Forge> = if use_forge { Some(&cli_forge) } else { None };

    let mut worst = Exit::Ok;
    let mut first = true;
    for (name, entry) in chosen {
        let report = status::gather(
            &name,
            &entry,
            &store,
            &status::Options {
                probe,
                forge,
                registry: Some(&registry),
            },
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            if !first {
                println!();
            }
            first = false;
            println!("{}", status::render(&report, verbose));
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
    let store = Store::open(default_state_path())?;
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

        // What the plan said it would include, so a cut cannot quietly differ from it.
        let all = release::carried_branches(&opened, entry.trunk(), &scheme)?;
        let names: Vec<String> = all.iter().map(|(branch, _)| branch.clone()).collect();
        let (chosen, left_out) = store.release_membership(&repo, &names);
        report_left_out(&repo, &left_out);
        if let Some(name) = cut_name {
            let mut carried: Vec<_> = all
                .iter()
                .filter(|(branch, _)| chosen.contains(branch))
                .cloned()
                .collect();
            // The upstream trunk is a parent of every cut. Without it a release held only
            // whatever upstream its branches were based on, so dropping a branch whose
            // pull request had merged took the change out of the release with it: the
            // merge commit lived upstream and had never been merged in here.
            let trunk_name = entry.upstream_trunk();
            let trunk = opened.resolve_commit(&trunk_name)?;
            carried.insert(0, (trunk_name, trunk.clone()));
            if let Some(orphaned) = check_orphan_commits_before_cut(&opened, &entry, trunk.clone())?
                && let Some(exit) = report_orphaned_cut(&repo, &orphaned, allow_drop)
            {
                return Ok(exit);
            }
            let request = cut_request(name.clone(), &carried);
            let previous_commit =
                release::previous_release_for_cut(&opened, &entry, &opened.bookmark_tips()?)
                    .map(|(_, commit)| commit);
            let created = release::build_cut(&entry.path, &request, previous_commit.as_ref())?;
            let member_tips: Vec<_> = carried.iter().skip(1).cloned().collect();
            let audit = match release::audit_cut(
                &entry.path,
                &member_tips,
                &created,
                release::AuditContext {
                    previous: previous_commit.as_ref(),
                    trunk: &trunk,
                },
            ) {
                Ok(audit) => audit,
                Err(error) => {
                    let _ =
                        knives::jj::abandon_commits(&entry.path, std::slice::from_ref(&created));
                    return Err(error);
                }
            };
            for name in &audit.inconclusive {
                println!(
                    "  {name}: content check inconclusive (replay conflicted; \
                     re-check after resolving the cut's conflicts)"
                );
            }
            if !audit.passed() {
                for name in &audit.missing {
                    println!(
                        "  !! {name}: the cut tree is missing or diverges from the member's content"
                    );
                }
                for file in &audit.unexplained {
                    println!(
                        "  !! {file}: changed between the previous release and this cut \
                         with no member or trunk explaining it"
                    );
                }
                knives::jj::abandon_commits(&entry.path, std::slice::from_ref(&created))?;
                println!(
                    "{repo}: cut abandoned; nothing was named or pushed. Fix the inputs and re-cut."
                );
                return Ok(Exit::Incomplete);
            }
            release::name_cut(&entry.path, &request.name, &created, &scheme)?;
            worst = worst.worst(report_completed_cut(
                &repo,
                &entry,
                &opened,
                &CompletedCut {
                    name: &name,
                    request: &request,
                    carried: &carried,
                    created: &created,
                    audit: &audit,
                },
            )?);
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

fn report_left_out(repo: &RepoName, left_out: &[(String, String)]) {
    if left_out.is_empty() {
        return;
    }
    println!(
        "{repo}: {} branch(es) left out of the release",
        left_out.len()
    );
    for (branch, why) in left_out {
        println!("  {branch}  {why}");
    }
}

struct CompletedCut<'a> {
    name: &'a str,
    request: &'a release::Cut,
    carried: &'a [(String, knives::ids::CommitId)],
    created: &'a knives::ids::CommitId,
    audit: &'a release::CutAudit,
}

fn report_completed_cut(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    opened: &knives::jj::Repo,
    cut: &CompletedCut<'_>,
) -> anyhow::Result<Exit> {
    let mut post_cut_exit = if cut.audit.inconclusive.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    };
    print_previous_release_position(opened, entry);
    println!(
        "  cut {} as {} with {} parent(s), flat, not pushed",
        cut.name,
        cut.created.as_str().chars().take(12).collect::<String>(),
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
    let carried_names: Vec<String> = cut
        .carried
        .iter()
        .map(|(branch, _)| branch.clone())
        .collect();
    let orphans = release::workspaces_to_clean(&present, &carried_names);
    if orphans.is_empty() {
        println!("  no workspaces left by dropped branches");
    } else {
        println!(
            "  {} workspace(s) belong to branches this cut dropped: {}",
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
        println!(
            "    {}",
            commit.as_str().chars().take(12).collect::<String>()
        );
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
    let Some(previous) = release::previous_release_for_cut(opened, entry, &tips) else {
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
            &commit.as_str()[..12.min(commit.as_str().len())]
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
    for (repo, entry) in chosen {
        let report = preflight::gather(&repo, &entry, &mut store, &forge);
        println!("{}", preflight::render(&report));
        worst = worst.worst(preflight::exit_for(&report));
    }
    store.save()?;
    Ok(worst)
}

fn run_sync(
    requested: Option<&str>,
    all: bool,
    json: bool,
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

    let mut worst = Exit::Ok;
    for (name, entry) in chosen {
        let report = sync::sync_repo(&name, &entry, &mut store, forge)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
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
