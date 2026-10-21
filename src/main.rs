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
mod ground;
mod release_carries;
mod release_cut;
mod release_edit;
mod release_rebase;

use std::process::ExitCode;

use branch_verbs::{FinishOptions, run_depends, run_finish, run_track};
use clap::Parser as _;
use ground::{Ground, Scope, Selected, grounded, placed, scan_home};
use knives::bind::Fork;
use knives::cli::{Cli, Command, Exit, LedgerAction, Output, ReleaseAction};
use knives::commands::claim::{Bound, current_identity};
use knives::commands::{
    audit, consumers, hook, ledger, notch, pr, preflight, pushed, register, repos, start, status,
    sync,
};
use knives::config::{Registry, default_config_path, load};
use knives::forge::Forge;
use knives::forge::github::CliForge;
use knives::ids::{BranchName, UpstreamName};
use knives::ledger::{Ledger, Scribe};
use knives::store::{Store, default_state_path};
use release_carries::{run_release_census, run_release_members, run_revision_carries};
use release_cut::{ReleaseInvocation, run_reap, run_release};
use release_edit::{ReleaseEdit, run_release_edit};
use release_rebase::run_rebase;

fn main() -> ExitCode {
    let exit = match dispatch() {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(error) => {
            eprintln!("knives: {error:#}");
            ExitCode::from(Exit::Incomplete.code())
        }
    };
    // Once, after whatever the command wrote, however it ended: one hand-off
    // per process rather than per entry, and every write path reaches it.
    if knives::ledger::appended() {
        knives::ledger_sweep::hand_off();
    }
    exit
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive command dispatch is easier to audit as one table"
)]
fn dispatch() -> anyhow::Result<Exit> {
    let cli = Cli::parse();
    let loaded = load(&default_config_path());
    let output = knives::cli::output_format(cli.json, cli.text);
    match cli.command {
        // These act on no fork: a hook event spawns nothing here (the hook
        // binds what its event names), and a vanished cwd is none of their
        // problem. `repos` still takes the sighting — asking what is
        // maintained from inside a fork is a sighting of it — but nothing
        // about it can fail the listing.
        Command::Hook { harness } => Ok(hook::run(harness, loaded)),
        Command::Register { repo } => register::run(repo, &loaded?),
        Command::Repos => {
            let _ = grounded(&loaded);
            let Some(home) = scan_home() else {
                return Ok(Exit::Usage);
            };
            repos::run(&loaded?, &home, output)
        }
        Command::Gh { args } => match knives::commands::gh::run(&args)? {},
        Command::Consumers { fork, consumer } => {
            let Some(fork) = grounded(&loaded)?.one_fork(fork.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_consumers(&fork, &consumer, output)
        }
        Command::Pushed { branches, repo } => {
            let ground = grounded(&loaded)?;
            let Some(fork) = ground.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pushed(ground.registry, &fork, &branches, output)
        }
        Command::Audit {
            repo,
            all,
            no_github,
        } => run_audit(
            grounded(&loaded)?,
            Scope {
                requested: repo.as_deref(),
                all,
            },
            output,
            !no_github,
        ),
        Command::Status {
            repo,
            all,
            verbose,
            no_landed,
            no_github,
        } => run_status(
            grounded(&loaded)?,
            StatusView {
                scope: Scope {
                    requested: repo.as_deref(),
                    all,
                },
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
            let Some(fork) = grounded(&loaded)?.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pr(&fork, number, timeline, output)
        }
        Command::Sync {
            repo,
            all,
            no_github,
        } => run_sync(
            grounded(&loaded)?,
            Scope {
                requested: repo.as_deref(),
                all,
            },
            output,
            !no_github,
        ),
        Command::Start {
            branch,
            repo,
            why,
            force,
        } => {
            let ground = grounded(&loaded)?;
            let (Some(fork), Some(branch)) =
                (ground.one_fork(repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            start::run(&fork, &branch, why.as_deref(), force, ground.bound())
        }
        Command::Finish {
            branch,
            repo,
            no_cleanup,
            superseded_by,
            force,
            why,
        } => {
            let ground = grounded(&loaded)?;
            let (Some(fork), Some(branch)) =
                (ground.one_fork(repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            run_finish(
                &fork,
                &branch,
                &FinishOptions {
                    superseded_by: superseded_by.as_deref(),
                    cleanup: !no_cleanup,
                    force,
                    why: why.as_deref(),
                },
                ground.bound(),
            )
        }
        Command::Track {
            branch,
            pr,
            fork_only,
            forget,
            repo,
        } => {
            let ground = grounded(&loaded)?;
            let (Some(fork), Some(branch)) =
                (ground.one_fork(repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            run_track(&fork, &branch, pr, fork_only, forget, ground.bound())
        }
        Command::Depends { branch, on, repo } => {
            let ground = grounded(&loaded)?;
            let (Some(fork), Some(branch)) =
                (ground.one_fork(repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            run_depends(&fork, &branch, &on, ground.bound())
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
            let ground = grounded(&loaded)?;
            let Some(fork) = ground.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            notch::run(
                &notch::Request {
                    fork: &fork,
                    bound: ground.bound(),
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
        Command::Ledger { action } => match action {
            LedgerAction::Sweep => ledger::run_sweep(output, loaded),
            LedgerAction::Migrate { dry_run } => ledger::run_migrate(output, dry_run, loaded),
        },
        Command::Preflight { repo } => {
            let ground = grounded(&loaded)?;
            let Some(fork) = ground.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_preflight(ground.registry, &fork)
        }
        Command::Release {
            action,
            repo,
            consumer,
            series,
        } => {
            let ground = grounded(&loaded)?;
            let Some(fork) = ground.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            let selected = match series.as_deref().map(|name| fork.entry.for_series(name)) {
                None => None,
                Some(Ok(entry)) => Some(entry),
                Some(Err(message)) => {
                    eprintln!("{}: {message}", fork.name);
                    return Ok(Exit::Usage);
                }
            };
            let fork = match &selected {
                Some(entry) => knives::bind::Fork { entry, ..fork },
                None => fork,
            };
            // Membership needs every series' base; one missing would hand its
            // branches to whichever series is left, so nothing proceeds.
            let unresolved = knives::release_model::unresolved_bases(
                &knives::jj::Repo::open(&fork.checkout.path)?,
                fork.entry,
            );
            if !unresolved.is_empty() {
                println!(
                    "{}: release series base {} does not resolve; fetch it, or set the series' \
                     base_remote to the remote that holds it, before any release verb",
                    fork.name,
                    unresolved.join(", ")
                );
                return Ok(Exit::Incomplete);
            }
            let extra: Vec<&std::path::Path> =
                consumer.iter().map(std::path::PathBuf::as_path).collect();
            dispatch_release(&fork, action, &extra, output, ground.bound())
        }
    }
}

/// The branch a verb acts on, or `None` after saying why the name is not one.
fn branch_name(typed: &str) -> Option<BranchName> {
    match BranchName::parse(typed) {
        Ok(branch) => Some(branch),
        Err(reason) => {
            eprintln!("{reason}");
            None
        }
    }
}

/// Census the checkouts that consume one fork's releases.
fn run_consumers(
    fork: &Fork<'_>,
    extras: &[std::path::PathBuf],
    output: Output,
) -> anyhow::Result<Exit> {
    let mut slugs = fork.entry.consumers.clone();
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
fn run_pushed(
    registry: &Registry,
    fork: &Fork<'_>,
    branches: &[String],
    output: Output,
) -> anyhow::Result<Exit> {
    let pulled = knives::ledger_sweep::pull(registry, &[&fork.upstream]);
    let store = Store::open(
        default_state_path(),
        &registry.former_names(),
        &[&fork.upstream],
    )?;
    let mut report = pushed::gather(fork, &store, branches);
    report
        .problems
        .extend(pulled.problems_with_store(&store, &fork.upstream));
    if let Some(payload) = knives::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", pushed::render(&report));
    }
    Ok(pushed::exit_for(&report))
}

/// Reconcile every requested fork's remote refs, release records, and anonymous heads.
fn run_audit(
    ground: Ground<'_>,
    scope: Scope<'_>,
    output: Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let registry = ground.registry;
    let chosen = match ground.selected(scope)? {
        Ok(chosen) => chosen,
        Err(exit) => return Ok(exit),
    };
    let forks = placed(&chosen);
    let pulled = knives::ledger_sweep::pull(registry, &forks);
    let store = open_store_for(registry, &chosen, &forks)?;
    let cli_forge = CliForge;
    let forge = use_forge.then_some(&cli_forge as &dyn Forge);
    let cache_root = knives::forge_cache::cache_root();
    let workers = parallelism();
    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(chosen.len());
    for chosen in &chosen {
        let mut report = match chosen {
            Selected::Bound(fork) if store.unreadable(&fork.upstream).is_some() => {
                let mut report = audit::Report::new(fork.name.to_string());
                report
                    .problems
                    .extend(unreadable_problem(&store, &fork.upstream));
                report
            }
            Selected::Bound(fork) => audit::gather(&audit::AuditInput {
                fork,
                store: &store,
                forge,
                cache_root: cache_root.as_deref(),
                workers,
            }),
            Selected::Unplaced { name, problem, .. } => {
                let mut report = audit::Report::new(name.to_string());
                report.problems.push(problem.clone());
                report
            }
        };
        let upstream = chosen.upstream();
        report
            .problems
            .extend(pulled.problems_with_store(&store, &upstream));
        worst = worst.worst(audit::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, scope.all, &reports, |reports| {
        knives::cli::joined(reports, "\n", audit::render)
    })?;
    Ok(worst)
}

/// Plan, curate or cut a release.
///
/// `bound` is the registry and the entry the current directory is inside;
/// every action pulls the ledger through the registry's destinations, and
/// one that writes the ledger derives who is acting from both.
#[allow(
    clippy::too_many_arguments,
    reason = "the fork, the action, the extra consumers, the output format and the cwd binding are independent inputs"
)]
fn dispatch_release(
    fork: &Fork<'_>,
    action: Option<ReleaseAction>,
    extra_consumers: &[&std::path::Path],
    output: Output,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    match action {
        None => run_release(fork, extra_consumers, &ReleaseInvocation::Plan, bound),
        Some(ReleaseAction::Cut { name, allow_drop }) => run_release(
            fork,
            extra_consumers,
            &ReleaseInvocation::Cut { name, allow_drop },
            bound,
        ),
        Some(ReleaseAction::Rebase { reference, no_drop }) => {
            let cache_root = knives::forge_cache::cache_root();
            run_rebase(
                fork,
                reference.as_deref(),
                no_drop,
                extra_consumers,
                cache_root.as_deref(),
                bound,
            )
        }
        Some(ReleaseAction::Members {
            reference,
            verify,
            carries,
            census,
            no_github,
        }) => {
            // The parser keeps `--census` apart from REF, `--carries` and
            // `--verify`, so a census request never has a target to lose here.
            match (census, carries.as_deref()) {
                (true, _) => run_release_census(fork, no_github, output),
                (false, Some(revision)) => {
                    run_revision_carries(fork, revision, reference.as_deref(), output)
                }
                (false, None) => {
                    run_release_members(fork, bound.registry, reference.as_deref(), verify, output)
                }
            }
        }
        Some(ReleaseAction::Reap) => run_reap(fork),
        Some(ReleaseAction::Include { branch, why }) => run_release_edit(
            fork,
            extra_consumers,
            &ReleaseEdit::Include { branch, why },
            bound,
        ),
        Some(ReleaseAction::Drop { branch, why }) => run_release_edit(
            fork,
            extra_consumers,
            &ReleaseEdit::Drop { branch, why },
            bound,
        ),
        Some(ReleaseAction::Advance { branches, from }) => {
            let branches = branches.into_iter().map(BranchName::new).collect();
            run_release_edit(
                fork,
                extra_consumers,
                &ReleaseEdit::Advance { branches, from },
                bound,
            )
        }
    }
}

/// The ledger writer for a command acting on `fork`, from the fork the current
/// directory is `bound` to.
///
/// The owner is resolved exactly as a claim's is, so one agent's events and its
/// claims carry the same name and a reader can join them.
fn scribe_for(fork: &Fork<'_>, bound: Bound<'_>) -> anyhow::Result<Scribe> {
    Ok(Scribe::new(
        Ledger::for_fork(fork),
        fork.upstream.clone(),
        fork.checkout.path.clone(),
        current_identity(bound)?.owner,
    ))
}

/// The store a many-fork report reads: one fork's unreadable ledger is that
/// fork's row ([`Store::open_each`]) when there is more than one row, and the
/// command's failure when there is one, as every single-fork command fails.
fn open_store_for(
    registry: &Registry,
    chosen: &[Selected<'_>],
    forks: &[&UpstreamName],
) -> anyhow::Result<Store> {
    let former = registry.former_names();
    Ok(if chosen.len() > 1 {
        Store::open_each(default_state_path(), &former, forks)?
    } else {
        Store::open(default_state_path(), &former, forks)?
    })
}

/// [`open_store_for`] for a report that writes the state file.
fn open_store_for_update(
    registry: &Registry,
    chosen: &[Selected<'_>],
    forks: &[&UpstreamName],
) -> anyhow::Result<Store> {
    let former = registry.former_names();
    Ok(if chosen.len() > 1 {
        Store::open_each_for_update(default_state_path(), &former, forks)?
    } else {
        Store::open_for_update(default_state_path(), &former, forks)?
    })
}

/// The problem a fork's row carries when its ledger could not be read.
fn unreadable_problem(store: &Store, upstream: &UpstreamName) -> Option<String> {
    store
        .unreadable(upstream)
        .map(|error| format!("ledger unavailable: {error}"))
}

/// How a report is produced and shown.
#[derive(Debug, Clone, Copy)]
struct StatusView<'a> {
    scope: Scope<'a>,
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
    fork: &Fork<'_>,
    number: u64,
    timeline: bool,
    output: knives::cli::Output,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let report = pr::gather(&pr::Request {
        fork,
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

/// One repository's row in the status document, with the timings of the gather
/// that produced it when one did.
///
/// A repository that cannot be gathered is still a row: its report carries the
/// error as a problem, so one broken registry entry does not swallow every other
/// repository's answer. An entry the scan did not place carries the scan's
/// reasons instead, and nothing is opened for it.
fn status_row(
    chosen: &Selected<'_>,
    gather: impl FnOnce(&Fork<'_>) -> anyhow::Result<(status::Report, status::Timings)>,
) -> (status::Report, Option<status::Timings>) {
    match chosen {
        Selected::Bound(fork) => match gather(fork) {
            Ok((report, timings)) => (report, Some(timings)),
            Err(error) => (
                status::Report {
                    repo: fork.name.to_string(),
                    trunk: fork.entry.trunk().to_owned(),
                    problems: vec![format!("could not gather: {error:#}")],
                    ..status::Report::default()
                },
                None,
            ),
        },
        Selected::Unplaced {
            name,
            entry,
            problem,
        } => (
            status::Report {
                repo: name.to_string(),
                trunk: entry.trunk().to_owned(),
                problems: vec![problem.clone()],
                ..status::Report::default()
            },
            None,
        ),
    }
}

fn run_status(ground: Ground<'_>, view: StatusView<'_>) -> anyhow::Result<Exit> {
    let StatusView {
        scope,
        gather: Gather { probe, use_forge },
        display: Display { verbose, output },
    } = view;
    let registry = ground.registry;
    let chosen = match ground.selected(scope)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let forks = placed(&chosen);
    let pulled = knives::ledger_sweep::pull(registry, &forks);
    let store = open_store_for(registry, &chosen, &forks)?;
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
    let rows: Vec<(status::Report, Option<status::Timings>)> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(repo_workers);
        for slice in chosen.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|chosen| {
                            status_row(chosen, |fork| {
                                if let Some(problem) = unreadable_problem(store, &fork.upstream) {
                                    return Ok((
                                        status::Report {
                                            repo: fork.name.to_string(),
                                            trunk: fork.entry.trunk().to_owned(),
                                            problems: vec![problem],
                                            ..status::Report::default()
                                        },
                                        status::Timings::default(),
                                    ));
                                }
                                status::gather_timed(
                                    fork,
                                    store,
                                    &status::Options {
                                        probe,
                                        forge,
                                        cache,
                                        registry: Some(registry),
                                        workers: probe_workers,
                                    },
                                )
                            })
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
                        .map(|chosen| {
                            status_row(chosen, |fork| {
                                Err(anyhow::anyhow!("gathering {} panicked", fork.name))
                            })
                        })
                        .collect()
                })
            })
            .collect()
    });

    // One document per invocation: an array under `--all`, the object otherwise.
    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(rows.len());
    let backlog = pulled.backlog();
    for ((mut report, timings), chosen) in rows.into_iter().zip(&chosen) {
        let upstream = chosen.upstream();
        report.problems.extend(pulled.problems_for(&upstream));
        report.notes.extend(backlog.for_fork(&upstream));
        // stderr, so a timed run's stdout is still the report a script parses.
        if let Some(timings) = timings
            && knives::timing::enabled()
        {
            eprintln!("{}", timings.line(&report.repo));
        }
        worst = worst.worst(status::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, scope.all, &reports, |reports| {
        knives::cli::joined(reports, "\n\n", |report| {
            status::render::render(report, verbose)
        })
    })?;
    Ok(worst)
}

fn run_preflight(registry: &Registry, fork: &Fork<'_>) -> anyhow::Result<Exit> {
    // Convention digests and claims only: no branch statement is asked about.
    let mut store = Store::open_for_update(default_state_path(), &registry.former_names(), &[])?;
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let report = preflight::gather(preflight::GatherInput {
        fork,
        store: &mut store,
        forge: &forge,
        cache: cache_root.as_deref(),
    });
    println!("{}", preflight::render(&report));
    store.save()?;
    Ok(preflight::exit_for(&report))
}

fn run_sync(
    ground: Ground<'_>,
    scope: Scope<'_>,
    output: knives::cli::Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let registry = ground.registry;
    let upstream = ground.bound().upstream.cloned();
    let chosen = match ground.sync_targets(scope)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let bound = Bound {
        registry,
        upstream: upstream.as_ref(),
    };
    let forks = placed(&chosen);
    let pulled = knives::ledger_sweep::pull(registry, &forks);
    let mut store = open_store_for_update(registry, &chosen, &forks)?;
    let cli_forge = CliForge;
    let forge = use_forge.then_some(&cli_forge as &dyn Forge);
    let cache_root = knives::forge_cache::cache_root();

    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(chosen.len());
    for chosen in &chosen {
        // A repository that cannot be synced is still a row in the document,
        // carrying the error as a problem; the repositories before it already
        // fetched and wrote their events, and their rows are not lost to it.
        let mut report = match chosen {
            Selected::Bound(fork) if store.unreadable(&fork.upstream).is_some() => sync::Report {
                repo: fork.name.to_string(),
                problems: unreadable_problem(&store, &fork.upstream)
                    .into_iter()
                    .collect(),
                ..sync::Report::default()
            },
            Selected::Bound(fork) => scribe_for(fork, bound)
                .and_then(|scribe| {
                    sync::sync_repo(sync::SyncInput {
                        fork,
                        store: &mut store,
                        forge,
                        scribe: &scribe,
                        cache: cache_root.as_deref(),
                    })
                })
                .unwrap_or_else(|error| sync::Report {
                    repo: fork.name.to_string(),
                    problems: vec![format!("could not sync: {error:#}")],
                    ..sync::Report::default()
                }),
            Selected::Unplaced { name, problem, .. } => sync::Report {
                repo: name.to_string(),
                problems: vec![problem.clone()],
                ..sync::Report::default()
            },
        };
        let upstream = chosen.upstream();
        report
            .problems
            .extend(pulled.problems_with_store(&store, &upstream));
        worst = worst.worst(sync::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, scope.all, &reports, |reports| {
        knives::cli::joined(reports, "\n", sync::render)
    })?;
    Ok(worst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_budget_caps_large_repository_sets_under_the_forge_limit() {
        let (repo_workers, probe_workers) = worker_budget(101, 101);

        assert_eq!(repo_workers, MAX_REPO_WORKERS);
        assert!(repo_workers <= MAX_REPO_WORKERS);
        assert!(probe_workers >= 1);
    }
}
