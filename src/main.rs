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
use knives::bind::{Fork, Unbound};
use knives::cli::{Cli, Command, Exit, Output, ReleaseAction};
use knives::commands::claim::current_identity;
use knives::commands::{
    audit, consumers, hook, notch, pr, preflight, pushed, register, repos, start, status, sync,
};
use knives::config::{
    ConfigError, NO_HOME, Registry, RepoEntry, default_config_path, home_dir, load,
};
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
    let loaded = load(&default_config_path());
    let output = knives::cli::output_format(cli.json, cli.text);
    match cli.command {
        // These act on no fork: a hook event spawns nothing here (the hook
        // binds what its event names), and a vanished cwd is none of their
        // problem. `repos` still takes the sighting — asking what is
        // maintained from inside a fork is a sighting of it — but nothing
        // about it can fail the listing.
        Command::Hook { harness } => Ok(hook::run(harness)),
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
            let Some(fork) = grounded(&loaded)?.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_pushed(&fork, &branches, output)
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
            run_depends(ground.registry, &fork, &branch, &on, ground.bound())
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
        Command::Preflight { repo } => {
            let Some(fork) = grounded(&loaded)?.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            run_preflight(&fork)
        }
        Command::Release {
            action,
            repo,
            consumer,
        } => {
            let ground = grounded(&loaded)?;
            let Some(fork) = ground.one_fork(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            let extra: Vec<&std::path::Path> =
                consumer.iter().map(std::path::PathBuf::as_path).collect();
            dispatch_release(&fork, action, &extra, output, ground.bound())
        }
    }
}

/// The scan root, or `None` after saying that there is none (exit `Usage`).
fn scan_home() -> Option<std::path::PathBuf> {
    let home = home_dir();
    if home.is_none() {
        eprintln!("{NO_HOME}");
    }
    home
}

/// The fork verbs' shared step, taken once per invocation by a verb that acts
/// on a fork: the current directory bound against the registry, and the
/// sighting recorded from that bind — one remotes read for both and the verb.
/// The sighting never fails a call; a verb that writes resolves who is acting
/// where it writes, from the same binding ([`Ground::bound`]).
fn grounded(loaded: &Result<Registry, ConfigError>) -> anyhow::Result<Ground<'_>> {
    let cwd = std::env::current_dir();
    let ground = match (&cwd, loaded) {
        (Ok(cwd), Ok(registry)) => Ok(Ground::new(registry, cwd)),
        (Err(error), _) => Err(anyhow::anyhow!("reading the current directory: {error}")),
        (Ok(_), Err(error)) => Err(anyhow::anyhow!("{error}")),
    };
    let bound = ground.as_ref().ok().and_then(Ground::bound);
    if let (Ok(cwd), Ok(identity)) = (&cwd, current_identity(bound)) {
        knives::seen::record_observation(bound, cwd, &identity);
    }
    ground
}

/// What every fork verb shares: the registry, and the current directory bound
/// against it once.
struct Ground<'a> {
    registry: &'a Registry,
    /// Exactly what `bind::here` said, so the verb reports a refusal as it
    /// always has, without asking the VCS again.
    here: Result<Fork<'a>, Unbound>,
}

impl<'a> Ground<'a> {
    fn new(registry: &'a Registry, cwd: &std::path::Path) -> Self {
        Self {
            registry,
            here: knives::bind::here(registry, cwd),
        }
    }

    /// The entry the current directory is inside, when it is inside one: what
    /// `current_identity` derives a terminal user's name from.
    fn bound(&self) -> Option<&RepoName> {
        self.here.as_ref().ok().map(|fork| &fork.name)
    }

    /// The fork `name` is: the current directory's when it is that entry, else
    /// the scan's; `None` after printing why not (exit `Usage`).
    ///
    /// The current directory matters only when it bound to `name`. However it
    /// failed to bind — outside a repository, a git clone, a checkout whose
    /// remotes cannot be read — the verb was asked about `name`, and the scan
    /// answers for it.
    fn named(&self, name: &str) -> Option<Fork<'a>> {
        let registry = self.registry;
        let name = RepoName::new(name);
        let home = scan_home()?;
        match knives::bind::resolve(registry, &name, self.here.as_ref().ok(), &home) {
            Ok(fork) => Some(fork),
            Err(why) => {
                eprintln!("{}", why.message(&name, registry));
                None
            }
        }
    }

    /// The single fork a verb acts on: named, or the one the current directory
    /// is inside; `None` after printing why not (exit `Usage`). A current
    /// directory whose remotes cannot be read is an error (exit `Incomplete`).
    ///
    /// Requiring the name on every command is absurd when you are inside the
    /// repository, and it was the loudest complaint about using this thing.
    fn one_fork(&self, requested: Option<&str>) -> anyhow::Result<Option<Fork<'a>>> {
        if let Some(name) = requested {
            return Ok(self.named(name));
        }
        match &self.here {
            Ok(fork) => Ok(Some(fork.clone())),
            Err(Unbound::Unreadable(error)) => Err(error.clone().into()),
            Err(unbound) => {
                eprintln!("{}", unbound.message(self.registry));
                Ok(None)
            }
        }
    }

    /// Every entry a many-repo verb covers, bound where the scan could.
    ///
    /// A name wins. Otherwise the repo you are standing in, because that is
    /// nearly always what you meant, and reporting on ten repositories at once
    /// is how `status` became unreadable. `--all` asks for all of them
    /// explicitly, and standing outside every managed repo also means all of
    /// them, since there is nothing else it could mean. Standing in a git clone
    /// is refused as every fork verb refuses it, and a checkout whose remotes
    /// cannot be read is an error, not a reason to sweep. An entry the scan
    /// found twice is still a row — rendered as a problem — and nothing is
    /// opened for it. What the scan could not read is said once, on stderr,
    /// whichever output format the document takes.
    fn selected(self, scope: Scope<'_>) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>> {
        let registry = self.registry;
        if registry.is_empty() {
            eprintln!(
                "no repos configured; add entries to {}",
                default_config_path().display()
            );
            return Ok(Err(Exit::Usage));
        }
        if let Some(name) = scope.requested {
            return Ok(self
                .named(name)
                .map_or(Err(Exit::Usage), |fork| Ok(vec![Selected::Bound(fork)])));
        }
        if !scope.all {
            match self.here {
                Ok(fork) => return Ok(Ok(vec![Selected::Bound(fork)])),
                Err(Unbound::Unreadable(error)) => return Err(error.into()),
                Err(unbound @ (Unbound::GitOnly { .. } | Unbound::NotColocated { .. })) => {
                    eprintln!("{}", unbound.message(registry));
                    return Ok(Err(Exit::Usage));
                }
                Err(_) => {}
            }
        }
        let Some(home) = scan_home() else {
            return Ok(Err(Exit::Usage));
        };
        let (selected, problems) = sweep(registry, &home);
        for problem in &problems {
            eprintln!("could not read: {problem}");
        }
        Ok(Ok(selected))
    }

    /// `sync` fetches and writes, so it never sweeps by accident: a name,
    /// `--all`, or the fork the current directory is inside; anything else is
    /// `Usage`, except a checkout whose remotes cannot be read, which is an
    /// error.
    fn sync_targets(self, scope: Scope<'_>) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>> {
        if scope.requested.is_some() || scope.all {
            return self.selected(scope);
        }
        match self.here {
            Ok(fork) => Ok(Ok(vec![Selected::Bound(fork)])),
            Err(Unbound::Unreadable(error)) => Err(error.into()),
            Err(unbound @ (Unbound::GitOnly { .. } | Unbound::NotColocated { .. })) => {
                eprintln!("{}", unbound.message(self.registry));
                Ok(Err(Exit::Usage))
            }
            Err(_) => {
                eprintln!("give a repo name, or --all");
                Ok(Err(Exit::Usage))
            }
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
    ground: Ground<'_>,
    scope: Scope<'_>,
    output: Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let chosen = match ground.selected(scope)? {
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
            Selected::Unplaced { name, problem, .. } => audit::Report {
                repo: name.to_string(),
                findings: Vec::new(),
                notes: Vec::new(),
                problems: vec![problem.clone()],
            },
        };
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
/// `bound` is the entry the current directory is inside; an action that writes
/// the ledger derives who is acting from it, the reports never ask.
#[allow(
    clippy::too_many_arguments,
    reason = "the fork, the action, the extra consumers, the output format and the cwd binding are independent inputs"
)]
fn dispatch_release(
    fork: &Fork<'_>,
    action: Option<ReleaseAction>,
    extra_consumers: &[&std::path::Path],
    output: Output,
    bound: Option<&RepoName>,
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

/// The ledger writer for a command acting on `fork`, from the entry the current
/// directory is `bound` to.
///
/// The owner is resolved exactly as a claim's is, so one agent's events and its
/// claims carry the same name and a reader can join them.
fn scribe_for(fork: &Fork<'_>, bound: Option<&RepoName>) -> anyhow::Result<Scribe> {
    Ok(Scribe::new(
        Ledger::for_repo(&fork.name),
        fork.name.clone(),
        fork.checkout.path.clone(),
        current_identity(bound)?.owner,
    ))
}

/// One registry entry as a many-repo verb sees it after the scan.
enum Selected<'a> {
    Bound(Fork<'a>),
    /// Found twice, or not found while the scan could not read something that
    /// may have been it: still a row, never opened. `problem` is the refusal;
    /// the scan's own complaints are reported once, not on every row.
    Unplaced {
        name: RepoName,
        entry: &'a RepoEntry,
        problem: String,
    },
}

/// Every entry the scan of `home` placed, and what the scan could not read,
/// for the caller to report once: each unplaced row carries only its own
/// refusal, whether or not every entry was found.
///
/// An entry with no checkout under `home` is not on this machine, which is
/// not a problem with it: the registry is shared across machines that hold
/// different subsets. It is noted once on stderr and left out. Unless the
/// scan failed to read something — then the missing checkout may be the one
/// it could not read, and the entry stays a row carrying that refusal.
fn sweep<'a>(registry: &'a Registry, home: &std::path::Path) -> (Vec<Selected<'a>>, Vec<String>) {
    let mut scan = knives::bind::scan(registry, home);
    let selected = registry
        .repos
        .iter()
        .filter_map(|(name, entry)| {
            let name = RepoName::new(name.clone());
            if let Some(fork) = scan.found.remove(&name) {
                return Some(Selected::Bound(fork));
            }
            let why = scan.unplaced(&name, Vec::new());
            if matches!(why, knives::bind::Unresolved::Missing { .. }) && scan.problems.is_empty() {
                eprintln!("knives: {name}: not on this machine");
                return None;
            }
            Some(Selected::Unplaced {
                name: name.clone(),
                entry,
                problem: format!("could not gather: {}", why.message(&name, registry)),
            })
        })
        .collect();
    (selected, scan.problems)
}

/// Which repos a report covers: a named one, all of them, or (neither) the one
/// the current directory is inside.
#[derive(Debug, Clone, Copy)]
struct Scope<'a> {
    requested: Option<&'a str>,
    all: bool,
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
    for (report, timings) in rows {
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
    ground: Ground<'_>,
    scope: Scope<'_>,
    output: knives::cli::Output,
    use_forge: bool,
) -> anyhow::Result<Exit> {
    let bound = ground.bound().cloned();
    let chosen = match ground.sync_targets(scope)? {
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
            Selected::Bound(fork) => scribe_for(fork, bound.as_ref())
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
