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
use knives::bind::{Fork, Unresolved};
use knives::cli::{Cli, Command, Exit, Output, ReleaseAction};
use knives::commands::claim::current_identity;
use knives::commands::{
    audit, consumers, hook, notch, pr, preflight, pushed, register, repos, start, status, sync,
};
use knives::config::{Registry, RepoEntry, default_config_path, home_dir, load};
use knives::forge::Forge;
use knives::forge::github::CliForge;
use knives::ids::{BranchName, RepoName};
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
        Command::Register { repo } => register::run(repo),
        Command::Repos => repos::run(output),
        Command::Consumers { fork, consumer } => {
            let registry = load(&default_config_path())?;
            let Some(fork) = one_fork(&registry, fork.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_consumers(&fork, &consumer, output)
        }
        Command::Pushed { branches, repo } => {
            let registry = load(&default_config_path())?;
            let Some(fork) = one_fork(&registry, repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pushed(&fork, &branches, output)
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
            let registry = load(&default_config_path())?;
            let Some(fork) = one_fork(&registry, repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pr(&fork, number, timeline, output)
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
            let registry = load(&default_config_path())?;
            let (Some(fork), Some(branch)) =
                (one_fork(&registry, repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            start::run(&fork, &branch, why.as_deref(), force)
        }
        Command::Finish {
            branch,
            repo,
            no_cleanup,
            superseded_by,
            force,
            why,
        } => {
            let registry = load(&default_config_path())?;
            let (Some(fork), Some(branch)) =
                (one_fork(&registry, repo.as_deref())?, branch_name(&branch))
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
            )
        }
        Command::Track {
            branch,
            pr,
            fork_only,
            forget,
            repo,
        } => {
            let registry = load(&default_config_path())?;
            let (Some(fork), Some(branch)) =
                (one_fork(&registry, repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            run_track(&fork, &branch, pr, fork_only, forget)
        }
        Command::Depends { branch, on, repo } => {
            let registry = load(&default_config_path())?;
            let (Some(fork), Some(branch)) =
                (one_fork(&registry, repo.as_deref())?, branch_name(&branch))
            else {
                return Ok(Exit::Usage);
            };
            run_depends(&registry, &fork, &branch, &on)
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
            let registry = load(&default_config_path())?;
            let Some(fork) = one_fork(&registry, repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            notch::run(
                &notch::Request {
                    fork: &fork,
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
            let registry = load(&default_config_path())?;
            let Some(fork) = one_fork(&registry, repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_preflight(&fork)
        }
        Command::Release {
            action,
            repo,
            consumer,
        } => {
            let registry = load(&default_config_path())?;
            let Some(fork) = one_fork(&registry, repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            let extra: Vec<&std::path::Path> =
                consumer.iter().map(std::path::PathBuf::as_path).collect();
            dispatch_release(&fork, action, &extra, output)
        }
        Command::Gh { args } => match knives::commands::gh::run(&args)? {},
    }
}

/// The registry's names, for a refusal that has to say what it does know.
fn known(registry: &Registry) -> String {
    registry
        .names()
        .map(|name| name.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The single fork a verb acts on: named, or the one the current directory is
/// inside; `None` after printing why not (exit `Usage`).
///
/// Requiring the name on every command is absurd when you are inside the
/// repository, and it was the loudest complaint about using this thing. Only an
/// unknown name gets the registry's names appended: the other refusals are
/// about a directory, and listing entries would not help find one.
fn one_fork<'a>(
    registry: &'a Registry,
    requested: Option<&str>,
) -> anyhow::Result<Option<Fork<'a>>> {
    let cwd = std::env::current_dir()?;
    let fork = if let Some(name) = requested {
        let name = RepoName::new(name);
        match knives::bind::resolve(registry, &name, &cwd, &home_dir())? {
            Ok(fork) => fork,
            Err(why @ Unresolved::Unknown) => {
                eprintln!("{}; known: {}", why.message(&name), known(registry));
                return Ok(None);
            }
            Err(why) => {
                eprintln!("{}", why.message(&name));
                return Ok(None);
            }
        }
    } else {
        match knives::bind::here(registry, &cwd)? {
            Ok(fork) => fork,
            Err(unbound) => {
                eprintln!("{}; known: {}", unbound.message(), known(registry));
                return Ok(None);
            }
        }
    };
    if !fork.checkout.is_jj() {
        eprintln!(
            "{} is a git clone, not a jj checkout; fork commands need jj",
            fork.checkout.path.display()
        );
        return Ok(None);
    }
    Ok(Some(fork))
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
fn run_pushed(fork: &Fork<'_>, branches: &[String], output: Output) -> anyhow::Result<Exit> {
    let store = Store::open(default_state_path())?;
    let report = pushed::gather(fork, &store, branches);
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
    let registry = load(&default_config_path())?;
    let chosen = match selected(&registry, requested, all)? {
        Ok(chosen) => chosen,
        Err(exit) => return Ok(exit),
    };
    let store = Store::open(default_state_path())?;
    let cli_forge = CliForge;
    let forge = use_forge.then_some(&cli_forge as &dyn Forge);
    let cache_root = knives::forge_cache::cache_root();
    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(chosen.len());
    for chosen in &chosen {
        let report = match chosen {
            Selected::Bound(fork) => audit::gather(&audit::AuditInput {
                fork,
                store: &store,
                forge,
                cache_root: cache_root.as_deref(),
            }),
            Selected::Unplaced { name, problems, .. } => audit::Report {
                repo: name.to_string(),
                findings: Vec::new(),
                notes: Vec::new(),
                problems: problems.clone(),
            },
        };
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
    fork: &Fork<'_>,
    action: Option<ReleaseAction>,
    extra_consumers: &[&std::path::Path],
    output: Output,
) -> anyhow::Result<Exit> {
    match action {
        None => run_release(fork, extra_consumers, &ReleaseInvocation::Plan),
        Some(ReleaseAction::Cut { name, allow_drop }) => run_release(
            fork,
            extra_consumers,
            &ReleaseInvocation::Cut { name, allow_drop },
        ),
        Some(ReleaseAction::Rebase { reference, no_drop }) => {
            let cache_root = knives::forge_cache::cache_root();
            run_rebase(
                fork,
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
        }) => run_release_carries(
            fork,
            CarriesInvocation {
                revision: revision.as_deref(),
                target: target.as_deref(),
                all,
                no_github,
                output,
            },
        ),
        Some(ReleaseAction::Members { reference, verify }) => run_release_members(
            fork,
            MembersInvocation {
                reference: reference.as_deref(),
                verify,
                output,
            },
        ),
        Some(ReleaseAction::Reap) => run_reap(fork),
        Some(ReleaseAction::Include { branch, why }) => {
            run_release_edit(fork, extra_consumers, &ReleaseEdit::Include { branch, why })
        }
        Some(ReleaseAction::Drop { branch, why }) => {
            run_release_edit(fork, extra_consumers, &ReleaseEdit::Drop { branch, why })
        }
        Some(ReleaseAction::Advance { branches, from }) => {
            let branches = branches.into_iter().map(BranchName::new).collect();
            run_release_edit(
                fork,
                extra_consumers,
                &ReleaseEdit::Advance { branches, from },
            )
        }
    }
}

/// The ledger writer for a command acting on `fork`.
///
/// The owner is resolved exactly as a claim's is, so one agent's events and its
/// claims carry the same name and a reader can join them.
fn scribe_for(fork: &Fork<'_>) -> anyhow::Result<Scribe> {
    let identity = knives::commands::claim::current_identity(&std::env::current_dir()?)?;
    Ok(Scribe::new(
        Ledger::for_repo(&fork.name),
        fork.name.clone(),
        fork.checkout.path.clone(),
        identity.owner,
    ))
}

/// One registry entry as a many-repo verb sees it after the scan.
enum Selected<'a> {
    Bound(Fork<'a>),
    /// Not found, or found twice: still a row, never opened. `problems` is the
    /// refusal followed by the scan's own complaints, so an unreadable checkout
    /// is named beside the entry it may have been.
    Unplaced {
        name: RepoName,
        entry: &'a RepoEntry,
        problems: Vec<String>,
    },
}

impl Selected<'_> {
    const fn name(&self) -> &RepoName {
        match self {
            Self::Bound(fork) => &fork.name,
            Self::Unplaced { name, .. } => name,
        }
    }
}

/// Every entry a many-repo verb covers, bound where the scan could.
///
/// A name wins. Otherwise the repo you are standing in, because that is nearly
/// always what you meant, and reporting on ten repositories at once is how
/// `status` became unreadable. `--all` asks for all of them explicitly, and
/// standing outside every managed repo also means all of them, since there is
/// nothing else it could mean. An entry the scan did not find, or found twice,
/// is still a row — rendered as a problem, exactly as an unopenable path was
/// before — and nothing is opened for it.
fn selected<'a>(
    registry: &'a Registry,
    requested: Option<&str>,
    all: bool,
) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>> {
    if registry.is_empty() {
        eprintln!(
            "no repos configured; add entries to {}",
            default_config_path().display()
        );
        return Ok(Err(Exit::Usage));
    }
    let cwd = std::env::current_dir()?;
    let home = home_dir();
    if let Some(name) = requested {
        let name = RepoName::new(name);
        return Ok(match knives::bind::resolve(registry, &name, &cwd, &home)? {
            Ok(fork) => Ok(vec![Selected::Bound(fork)]),
            Err(why @ Unresolved::Unknown) => {
                eprintln!("{}; known: {}", why.message(&name), known(registry));
                Err(Exit::Usage)
            }
            Err(why) => {
                eprintln!("{}", why.message(&name));
                Err(Exit::Usage)
            }
        });
    }
    if !all && let Ok(fork) = knives::bind::here(registry, &cwd)? {
        return Ok(Ok(vec![Selected::Bound(fork)]));
    }
    Ok(Ok(sweep(registry, &home)))
}

/// Every entry, bound through one scan of `home`.
fn sweep<'a>(registry: &'a Registry, home: &std::path::Path) -> Vec<Selected<'a>> {
    let mut scan = knives::bind::scan(registry, home);
    registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let name = RepoName::new(name.clone());
            if let Some(fork) = scan.found.remove(&name) {
                return Selected::Bound(fork);
            }
            let why = scan.duplicates.remove(&name).map_or_else(
                || Unresolved::Missing {
                    home: home.to_owned(),
                },
                |paths| Unresolved::Duplicate {
                    home: home.to_owned(),
                    paths,
                },
            );
            let problems = std::iter::once(format!("could not gather: {}", why.message(&name)))
                .chain(scan.problems.iter().cloned())
                .collect();
            Selected::Unplaced {
                name,
                entry,
                problems,
            }
        })
        .collect()
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

/// One registry entry's status, or why it could not be gathered.
type GatheredStatus<'a> = (
    &'a Selected<'a>,
    anyhow::Result<(status::Report, status::Timings)>,
);

fn run_status(requested: Option<&str>, view: StatusView) -> anyhow::Result<Exit> {
    let StatusView {
        scope: Scope { all },
        gather: Gather { probe, use_forge },
        display: Display { verbose, output },
    } = view;
    let registry = load(&default_config_path())?;
    let chosen = match selected(&registry, requested, all)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let store = Store::open(default_state_path())?;
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
                        .map(|chosen| {
                            let gathered = match chosen {
                                Selected::Bound(fork) => {
                                    let ledger = Ledger::for_repo(&fork.name);
                                    status::gather_timed(
                                        fork,
                                        store,
                                        &status::Options {
                                            probe,
                                            forge,
                                            cache,
                                            registry: Some(registry),
                                            ledger: Some(&ledger),
                                            workers: probe_workers,
                                        },
                                    )
                                }
                                Selected::Unplaced { .. } => Err(anyhow::anyhow!("not placed")),
                            };
                            (chosen, gathered)
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
                            (
                                chosen,
                                Err(anyhow::anyhow!("gathering {} panicked", chosen.name())),
                            )
                        })
                        .collect()
                })
            })
            .collect()
    });

    // One document per invocation: an array under `--all`, the object otherwise.
    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(gathered.len());
    for (chosen, gathered) in gathered {
        let report = status_row(chosen, gathered);
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

/// One repository's row in the status document.
///
/// A repository that cannot be gathered is still a row: its report carries the
/// error as a problem, so one broken registry entry does not swallow every other
/// repository's answer. An entry the scan did not place carries the scan's
/// reasons instead.
fn status_row(
    chosen: &Selected<'_>,
    gathered: anyhow::Result<(status::Report, status::Timings)>,
) -> status::Report {
    match (chosen, gathered) {
        (_, Ok((report, timings))) => {
            // stderr, so a timed run's stdout is still the report a script parses.
            if knives::timing::enabled() {
                eprintln!("{}", timings.line(chosen.name().as_str()));
            }
            report
        }
        (
            Selected::Unplaced {
                name,
                entry,
                problems,
            },
            Err(_),
        ) => status::Report {
            repo: name.to_string(),
            trunk: entry.trunk().to_owned(),
            problems: problems.clone(),
            ..status::Report::default()
        },
        (Selected::Bound(fork), Err(error)) => status::Report {
            repo: fork.name.to_string(),
            trunk: fork.entry.trunk().to_owned(),
            problems: vec![format!("could not gather: {error:#}")],
            ..status::Report::default()
        },
    }
}

fn run_preflight(fork: &Fork<'_>) -> anyhow::Result<Exit> {
    let mut store = Store::open_for_update(default_state_path())?;
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
    requested: Option<&str>,
    all: bool,
    output: knives::cli::Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let chosen = match sync_targets(&registry, requested, all)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut store = Store::open_for_update(default_state_path())?;
    let cli_forge = CliForge;
    let forge = use_forge.then_some(&cli_forge as &dyn Forge);
    let cache_root = knives::forge_cache::cache_root();

    let mut worst = Exit::Ok;
    let mut reports = Vec::with_capacity(chosen.len());
    for chosen in &chosen {
        // A repository that cannot be synced is still a row in the document,
        // carrying the error as a problem; the repositories before it already
        // fetched and wrote their events, and their rows are not lost to it.
        let report = match chosen {
            Selected::Bound(fork) => scribe_for(fork)
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
            Selected::Unplaced { name, problems, .. } => sync::Report {
                repo: name.to_string(),
                problems: problems.clone(),
                ..sync::Report::default()
            },
        };
        worst = worst.worst(sync::exit_for(&report));
        reports.push(report);
    }
    knives::cli::emit_reports(output, all, &reports, |reports| {
        knives::cli::joined(reports, "\n", sync::render)
    })?;
    Ok(worst)
}

/// `sync` fetches and writes, so it never sweeps by accident: a name, `--all`,
/// or the fork the current directory is inside; anything else is `Usage`.
fn sync_targets<'a>(
    registry: &'a Registry,
    requested: Option<&str>,
    all: bool,
) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>> {
    if requested.is_some() || all {
        return selected(registry, requested, all);
    }
    let cwd = std::env::current_dir()?;
    if let Ok(fork) = knives::bind::here(registry, &cwd)? {
        return Ok(Ok(vec![Selected::Bound(fork)]));
    }
    eprintln!("give a repo name, or --all");
    Ok(Err(Exit::Usage))
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
