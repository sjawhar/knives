//! The `knives` binary.
//!
//! Dispatch, the repository selection every command shares, and the report
//! verbs whose work is gather, exit code, render: `status`, `sync`, `audit`,
//! `consumers`, `pushed`, `pr`, `preflight`. The verbs that write — a cut, a
//! rebase, a membership edit, a branch handed back — each own a module here:
//! [`release_cut`], [`release_rebase`], [`release_edit`], [`branch_verbs`];
//! the release's content reports are [`release_carries`]. Every command
//! returns an [`Exit`], so the match stays a table.

#![allow(
    clippy::redundant_pub_crate,
    reason = "the verb modules are private to the binary; crate visibility is what their callers in the root have"
)]

mod branch_verbs;
mod release_carries;
mod release_cut;
mod release_edit;
mod release_rebase;

use std::process::ExitCode;

use branch_verbs::{FinishOptions, run_depends, run_finish, run_track};
use clap::Parser as _;
use knives::cli::{Cli, Command, Exit, Output, ReleaseAction};
use knives::commands::claim::current_identity;
use knives::commands::{
    audit, consumers, hook, init, notch, pr, preflight, pushed, register, repos, start, status,
    sync,
};
use knives::config::{default_config_path, load};
use knives::forge::Forge;
use knives::forge::github::CliForge;
use knives::ids::{BranchName, BranchTarget, RepoName};
use knives::ledger::{Ledger, Scribe};
use knives::store::{Store, default_state_path};
use release_carries::{
    CarriesInvocation, MembersInvocation, run_release_carries, run_release_members,
};
use release_cut::{ReleaseInvocation, run_reap, run_release};
use release_edit::{ReleaseEdit, run_release_edit};
use release_rebase::run_rebase;

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
    let _ = (|| -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let identity = current_identity(&cwd)?;
        knives::seen::record_observation(&cwd, &identity);
        Ok(())
    })();
    let output = knives::cli::output_format(cli.json, cli.text);
    match cli.command {
        Command::Hook { harness } => Ok(hook::run(harness)),
        Command::Init { repo } => init::run(repo),
        Command::Register { repo } => register::run(repo),
        Command::Repos => repos::run(output),
        Command::Consumers { fork, consumer } => {
            let Some(name) = one_repo(fork.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_consumers(&name, &consumer, output)
        }
        Command::Pushed { branches, repo } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pushed(&name, &branches, output)
        }
        Command::Audit {
            repo,
            all,
            no_github,
        } => run_audit(repo.as_deref(), all, output, !no_github),
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
        Command::Pr {
            number,
            repo,
            timeline,
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pr(&name, number, timeline, output)
        }
        Command::Sync {
            repo,
            all,
            no_github,
        } => run_sync(repo.as_deref(), all, output, !no_github),
        Command::Start {
            branch,
            repo,
            why,
            force,
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            start::run(&name, &BranchName::new(branch), why.as_deref(), force)
        }
        Command::Finish {
            branch,
            repo,
            no_cleanup,
            superseded_by,
            force,
            why,
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_finish(
                &BranchTarget::new(name, BranchName::new(branch)),
                &FinishOptions {
                    superseded_by: superseded_by.as_deref(),
                    cleanup: !no_cleanup,
                    force,
                    why: why.as_deref(),
                },
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
            disposition,
            dispositions,
            events,
            verify,
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
                    disposition: disposition.as_deref(),
                    dispositions,
                    events,
                    verify,
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
            dispatch_release(&chosen, action, &extra, output)
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

/// Census the checkouts that consume one fork's releases.
fn run_consumers(
    fork: &RepoName,
    extras: &[std::path::PathBuf],
    output: Output,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(fork) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!("unknown repo {fork}; known: {}", known.join(", "));
        return Ok(Exit::Usage);
    };
    let mut slugs = entry.consumers.clone();
    slugs.sort();
    slugs.dedup();
    let mut locals = extras.to_vec();
    locals.sort();
    locals.dedup();
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let report = consumers::gather(&consumers::Request {
        fork,
        entry,
        slugs: &slugs,
        locals: &locals,
        forge: &forge,
        cache_root: cache_root.as_deref(),
        heads: &heads,
    });
    if let Some(payload) = knives::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", consumers::render(&report));
    }
    Ok(consumers::exit_for(&report))
}

/// Reconcile one fork's local bookmarks with the live refs on their owning remotes.
fn run_pushed(repo: &RepoName, branches: &[String], output: Output) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!("unknown repo {repo}; known: {}", known.join(", "));
        return Ok(Exit::Usage);
    };
    let store = Store::open(default_state_path())?;
    let report = pushed::gather(repo, entry, &store, branches);
    if let Some(payload) = knives::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", pushed::render(&report));
    }
    Ok(pushed::exit_for(&report))
}

/// Reconcile every requested fork's remote refs, release records, and anonymous heads.
fn run_audit(
    requested: Option<&str>,
    all: bool,
    output: Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let chosen = match selected(requested, all)? {
        Ok(chosen) => chosen,
        Err(exit) => return Ok(exit),
    };
    let store = Store::open(default_state_path())?;
    let cli_forge = CliForge;
    let forge = use_forge.then_some(&cli_forge as &dyn Forge);
    let cache_root = knives::forge_cache::cache_root();
    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(chosen.len());
    for (repo, entry) in chosen {
        let report = audit::gather(&audit::AuditInput {
            repo: &repo,
            entry: &entry,
            store: &store,
            forge,
            cache_root: cache_root.as_deref(),
        });
        worst = worst.worst(audit::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, all, &reports, |reports| {
        knives::cli::joined(reports, "\n", audit::render)
    })?;
    Ok(worst)
}

/// Plan, curate or cut a release.
fn dispatch_release(
    chosen: &RepoName,
    action: Option<ReleaseAction>,
    extra_consumers: &[&std::path::Path],
    output: Output,
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
                extra_consumers,
                cache_root.as_deref(),
            )
        }
        Some(ReleaseAction::Carries {
            revision,
            target,
            all,
            no_github,
        }) => {
            let registry = load(&default_config_path())?;
            let Some(entry) = registry.get(chosen) else {
                return Ok(Exit::Usage);
            };
            run_release_carries(
                chosen,
                entry,
                CarriesInvocation {
                    revision: revision.as_deref(),
                    target: target.as_deref(),
                    all,
                    no_github,
                    output,
                },
            )
        }
        Some(ReleaseAction::Members { reference, verify }) => {
            let registry = load(&default_config_path())?;
            let Some(entry) = registry.get(chosen) else {
                return Ok(Exit::Usage);
            };
            run_release_members(
                chosen,
                entry,
                MembersInvocation {
                    reference: reference.as_deref(),
                    verify,
                    output,
                },
            )
        }
        Some(ReleaseAction::Reap) => run_reap(chosen.as_str()),
        Some(ReleaseAction::Include { branch, why }) => run_release_edit(
            chosen.as_str(),
            extra_consumers,
            &ReleaseEdit::Include { branch, why },
        ),
        Some(ReleaseAction::Drop { branch, why }) => run_release_edit(
            chosen.as_str(),
            extra_consumers,
            &ReleaseEdit::Drop { branch, why },
        ),
        Some(ReleaseAction::Advance { branches, from }) => {
            let branches = branches.into_iter().map(BranchName::new).collect();
            run_release_edit(
                chosen.as_str(),
                extra_consumers,
                &ReleaseEdit::Advance { branches, from },
            )
        }
    }
}

/// The ledger writer for a command acting on `entry`.
///
/// The owner is resolved exactly as a claim's is, so one agent's events and its
/// claims carry the same name and a reader can join them.
fn scribe_for(repo: &RepoName, entry: &knives::config::RepoEntry) -> anyhow::Result<Scribe> {
    let identity = knives::commands::claim::current_identity(&std::env::current_dir()?)?;
    Ok(Scribe::new(
        Ledger::for_repo(repo),
        repo.clone(),
        entry.path.clone(),
        identity.owner,
    ))
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

fn run_pr(
    repo: &RepoName,
    number: u64,
    timeline: bool,
    output: knives::cli::Output,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!("unknown repo {repo}; known: {}", known.join(", "));
        return Ok(Exit::Usage);
    };
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let report = pr::gather(&pr::Request {
        repo,
        entry,
        number,
        forge: &forge,
        cache_root: cache_root.as_deref(),
        timeline,
    })?;
    let Some(report) = report else {
        eprintln!("{repo}: the forge did not report #{number}");
        return Ok(Exit::Incomplete);
    };
    if let Some(payload) = knives::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", pr::render(&report));
    }
    Ok(pr::exit_for(&report))
}

/// One registry entry's status, or why it could not be gathered.
type GatheredStatus<'a> = (
    &'a (RepoName, knives::config::RepoEntry),
    anyhow::Result<(status::Report, status::Timings)>,
);

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
    let gathered: Vec<GatheredStatus<'_>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(repo_workers);
        for slice in chosen.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|chosen_entry| {
                            let (name, entry) = chosen_entry;
                            let ledger = Ledger::for_repo(name);
                            let gathered = status::gather_timed(
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
                            );
                            (chosen_entry, gathered)
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
                        .map(|chosen_entry| {
                            (
                                chosen_entry,
                                Err(anyhow::anyhow!("gathering {} panicked", chosen_entry.0)),
                            )
                        })
                        .collect()
                })
            })
            .collect()
    });

    // One document per invocation: an array under `--all`, the object otherwise.
    // A repository that cannot be gathered is still a row in the document: its
    // report carries the error as a problem, so one broken registry entry does
    // not swallow every other repository's answer.
    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(gathered.len());
    for ((name, entry), gathered) in gathered {
        let report = match gathered {
            Ok((report, timings)) => {
                // stderr, so a timed run's stdout is still the report a script parses.
                if knives::timing::enabled() {
                    eprintln!("{}", timings.line(name.as_str()));
                }
                report
            }
            Err(error) => status::Report {
                repo: name.to_string(),
                trunk: entry.trunk().to_owned(),
                problems: vec![format!("could not gather: {error:#}")],
                ..status::Report::default()
            },
        };
        worst = worst.worst(status::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, all, &reports, |reports| {
        knives::cli::joined(reports, "\n\n", |report| {
            status::render::render(report, verbose)
        })
    })?;
    Ok(worst)
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
    let mut reports = Vec::with_capacity(chosen.len());
    for (name, entry) in chosen {
        // A repository that cannot be synced is still a row in the document,
        // carrying the error as a problem; the repositories before it already
        // fetched and wrote their events, and their rows are not lost to it.
        let synced = scribe_for(&name, &entry).and_then(|scribe| {
            sync::sync_repo(sync::SyncInput {
                entry: &entry,
                store: &mut store,
                forge,
                scribe: &scribe,
                cache: cache_root.as_deref(),
            })
        });
        let report = synced.unwrap_or_else(|error| sync::Report {
            repo: name.to_string(),
            problems: vec![format!("could not sync: {error:#}")],
            ..sync::Report::default()
        });
        worst = worst.worst(sync::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, all, &reports, |reports| {
        knives::cli::joined(reports, "\n", sync::render)
    })?;
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
