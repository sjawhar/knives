//! The `knives` binary.
//!
//! Dispatch, plus the verbs that are one jj sequence over a release rather than
//! a report with a renderer: `cut`, `rebase`, `carries` and `members` decide
//! here, and the release edits in [`release_edit`]. Every other command owns
//! its own logic and returns an [`Exit`], so the match stays a table.

#[allow(
    clippy::redundant_pub_crate,
    reason = "a private module of the binary; crate visibility is what its callers in the root have"
)]
mod release_edit;

use std::process::ExitCode;

use clap::Parser as _;
use knives::carriage::{
    self, CarriesReport, CensusOptions, CensusReport, CheckInput, Target, TargetCheck, TargetRole,
};
use knives::cli::{Cli, Command, Exit, Output, ReleaseAction};
use knives::commands::claim::{
    ClaimContext, ClaimDecision, current_identity, decide, last_seen_provenance,
    render_claim_context,
};
use knives::commands::{
    audit, consumers, hook, init, notch, pr, preflight, pushed, register, release, repos, start,
    status, sync,
};
use knives::config::{default_config_path, load};
use knives::forge::github::CliForge;
use knives::forge::{Forge, PullRequest};
use knives::ids::{BranchName, BranchTarget, ReleaseScheme, RepoName, Requirement};
use knives::jj::Repo;
use knives::ledger::{Draft, Kind, Ledger, Scribe};
use knives::release_model::{
    BranchSuccessions, RecordedCut, StackedHistoryContext, carried_branches, carried_from_tips,
    last_recorded_cut, members_event_text, previous_release_for_cut, trunk_positions,
};
use knives::store::{Store, default_state_path};
use release_edit::{
    EditRecord, ReleaseEdit, record_edit_event, recorded_parents, release_is_locally_movable,
    run_release_edit,
};

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

#[derive(Clone, Copy)]
struct CarriesInvocation<'a> {
    revision: Option<&'a str>,
    target: Option<&'a str>,
    all: bool,
    no_github: bool,
    output: Output,
}

#[derive(Clone, Copy)]
struct MembersInvocation<'a> {
    reference: Option<&'a str>,
    verify: bool,
    output: Output,
}

/// Inspect a release's direct parents and, on request, replay their content.
fn run_release_members(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    request: MembersInvocation<'_>,
) -> anyhow::Result<Exit> {
    let opened = Repo::open(&entry.path)?;
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let reference = if let Some(reference) = request.reference {
        std::borrow::Cow::Borrowed(reference)
    } else {
        let consumers = release::ConsumerInputs {
            slugs: &entry.consumers,
            locals: &[],
            forge: &forge,
            cache_root: cache_root.as_deref(),
            heads: &heads,
        };
        let plan = release::plan(repo, entry, &consumers, &Ledger::for_repo(repo).entries()?)?;
        let Some(reference) = plan.release else {
            println!("{repo}: no release to inspect; cut one first");
            return Ok(Exit::Incomplete);
        };
        std::borrow::Cow::Owned(reference)
    };
    let report = release::gather_members(&opened, entry, &reference, request.verify)?;
    let exit = members_exit(&report, request.verify);
    print_members(&report, request.output)?;
    Ok(exit)
}

fn members_exit(report: &release::MembersReport, verify: bool) -> Exit {
    if !report.problems.is_empty() {
        Exit::Incomplete
    } else if verify
        && report
            .audit
            .as_ref()
            .is_some_and(|audit| !audit.missing.is_empty() || !audit.unexplained.is_empty())
    {
        Exit::Findings
    } else {
        Exit::Ok
    }
}

fn print_members(report: &release::MembersReport, output: Output) -> anyhow::Result<()> {
    if let Some(payload) = knives::cli::machine_payload(output, report)? {
        println!("{payload}");
    } else {
        println!("{}", release::render_members(report));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CarriesMode {
    Bare,
    ExplicitTarget,
}

/// Answer one revision's carriage, or census every maintained branch.
fn run_release_carries(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    request: CarriesInvocation<'_>,
) -> anyhow::Result<Exit> {
    if request.all {
        let forge = CliForge;
        let forge: Option<&dyn Forge> = (!request.no_github).then_some(&forge);
        let cache_root = knives::forge_cache::cache_root();
        let report = carriage::census(
            repo,
            entry,
            forge,
            CensusOptions {
                cache_root: cache_root.as_deref(),
                workers: parallelism(),
            },
        )?;
        let exit = census_exit(&report);
        print_census(&report, request.output)?;
        return Ok(exit);
    }
    let Some(revision) = request.revision else {
        return Ok(Exit::Usage);
    };
    run_revision_carries(repo, entry, request, revision)
}

fn run_revision_carries(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    request: CarriesInvocation<'_>,
    revision: &str,
) -> anyhow::Result<Exit> {
    let opened = Repo::open(&entry.path)?;
    let tip = match opened.resolve_commit(revision) {
        Ok(tip) => tip,
        Err(error) => {
            let report = carries_problem(
                repo,
                revision.to_owned(),
                format!("cannot resolve revision {revision}: {error}"),
            );
            print_carries(&report, request.output)?;
            return Ok(Exit::Incomplete);
        }
    };
    let trunk_name = entry.upstream_trunk();
    let trunk = match opened.resolve_commit(&trunk_name) {
        Ok(trunk) => trunk,
        Err(error) => {
            let report = carries_problem(
                repo,
                carries_revision(revision, &tip),
                format!("cannot resolve upstream trunk {trunk_name}: {error}"),
            );
            print_carries(&report, request.output)?;
            return Ok(Exit::Incomplete);
        }
    };
    let tips = opened.bookmark_tips()?;
    let all_targets = carriage::targets(
        &tips,
        &entry.release_scheme(),
        (trunk_name.as_str(), trunk),
        entry.publish_remote(),
    );
    let mode = request
        .target
        .map_or(CarriesMode::Bare, |_| CarriesMode::ExplicitTarget);
    let (mut selected, superseded) = match request.target {
        Some(target) => match selected_carries_targets(&opened, &all_targets, target) {
            Ok(targets) => (targets, Vec::new()),
            Err(error) => {
                let report = carries_problem(
                    repo,
                    carries_revision(revision, &tip),
                    format!("cannot resolve target {target}: {error}"),
                );
                print_carries(&report, request.output)?;
                return Ok(Exit::Incomplete);
            }
        },
        None => all_targets
            .into_iter()
            .partition(|target| target.role != TargetRole::SupersededRelease),
    };
    if let (Some(requested), Some(selected_target)) = (request.target, selected.first_mut())
        && requested == trunk_name
    {
        selected_target.role = TargetRole::UpstreamTrunk;
    }
    let checks = CarriesChecks {
        input: CheckInput {
            repo: &opened,
            revision,
            tip: &tip,
        },
        fallback: request.target.unwrap_or(trunk_name.as_str()),
    };
    let mut report = CarriesReport {
        repo: repo.to_string(),
        revision: carries_revision(revision, &tip),
        checks: Vec::with_capacity(selected.len() + superseded.len()),
        notes: Vec::new(),
        problems: Vec::new(),
    };
    checks.append(&mut report, selected);
    let exit = carries_exit(&mut report, &checks, superseded, mode);
    print_carries(&report, request.output)?;
    Ok(exit)
}

fn carries_exit(
    report: &mut CarriesReport,
    checks: &CarriesChecks<'_>,
    superseded: Vec<Target>,
    mode: CarriesMode,
) -> Exit {
    if !report.problems.is_empty() {
        return Exit::Incomplete;
    }
    match mode {
        CarriesMode::Bare => {
            if !carries_safe(report) {
                checks.append(report, superseded);
            }
            if carries_safe(report) {
                Exit::Ok
            } else {
                Exit::Findings
            }
        }
        CarriesMode::ExplicitTarget => {
            if report.checks.iter().all(|check| check.verdict.carried()) {
                Exit::Ok
            } else {
                Exit::Findings
            }
        }
    }
}

struct CarriesChecks<'a> {
    input: CheckInput<'a>,
    fallback: &'a str,
}

impl CarriesChecks<'_> {
    fn append(&self, report: &mut CarriesReport, targets: Vec<Target>) {
        for target in targets {
            let target_name = carriage::target_name(&target, self.fallback);
            match carriage::check(&self.input, &target) {
                Ok(check) => report.checks.push(TargetCheck {
                    target: target_name,
                    commit: target.commit,
                    role: target.role,
                    verdict: check.verdict,
                    evidence: check.evidence,
                }),
                Err(error) => report
                    .problems
                    .push(format!("cannot check {target_name}: {error}")),
            }
        }
    }
}

fn carries_safe(report: &CarriesReport) -> bool {
    report.checks.iter().any(|check| {
        check.verdict.carried()
            && matches!(
                check.role,
                TargetRole::LiveRelease | TargetRole::UpstreamTrunk
            )
    })
}

fn carries_problem(repo: &RepoName, revision: String, problem: String) -> CarriesReport {
    CarriesReport {
        repo: repo.to_string(),
        revision,
        checks: Vec::new(),
        notes: Vec::new(),
        problems: vec![problem],
    }
}

fn selected_carries_targets(
    opened: &Repo,
    known_targets: &[Target],
    target: &str,
) -> anyhow::Result<Vec<Target>> {
    let commit = opened.resolve_commit(target)?;
    let role = known_targets
        .iter()
        .find(|known| known.commit == commit)
        .map_or(TargetRole::SupersededRelease, |known| known.role);
    Ok(vec![Target {
        refs: Vec::new(),
        commit,
        role,
    }])
}

fn carries_revision(revision: &str, tip: &knives::ids::CommitId) -> String {
    format!(
        "{revision} @ {}",
        tip.as_str().chars().take(12).collect::<String>()
    )
}

fn print_carries(report: &CarriesReport, output: Output) -> anyhow::Result<()> {
    if let Some(payload) = knives::cli::machine_payload(output, report)? {
        println!("{payload}");
    } else {
        println!("{}", carriage::render_carries(report));
    }
    Ok(())
}

fn census_exit(report: &CensusReport) -> Exit {
    if !report.problems.is_empty() || report.rows.iter().any(|row| row.in_open_pull.is_none()) {
        Exit::Incomplete
    } else if report.orphans.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

fn print_census(report: &CensusReport, output: Output) -> anyhow::Result<()> {
    if let Some(payload) = knives::cli::machine_payload(output, report)? {
        println!("{payload}");
    } else {
        println!("{}", carriage::render_census(report));
    }
    Ok(())
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
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "rebasing is one ordered stateful gate sequence; its inputs remain explicit and its stages must not be separated"
)]
fn run_rebase(
    name: &str,
    reference: Option<&str>,
    no_drop: bool,
    extra_consumers: &[&std::path::Path],
    cache_root: Option<&std::path::Path>,
) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let mut worst = Exit::Ok;
    let forge = CliForge;
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    for (repo, entry) in chosen {
        let opened = knives::jj::Repo::open(&entry.path)?;
        let consumers = release::ConsumerInputs {
            slugs: &entry.consumers,
            locals: &locals,
            forge: &forge,
            cache_root,
            heads: &heads,
        };
        let plan = release::plan(
            &repo,
            &entry,
            &consumers,
            &Ledger::for_repo(&repo).entries()?,
        )?;
        if !plan.problems.is_empty() {
            println!("{}", release::render(&plan));
            worst = worst.worst(Exit::Incomplete);
            continue;
        }
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
            release::repair_effect(
                &plan.pins,
                knives::ids::BookmarkRef::parse(&release_name).branch(),
            ),
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
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    release_name: &str,
    destination: &RebaseDestination,
) -> anyhow::Result<Exit> {
    if destination.landed.is_empty() {
        return Ok(Exit::Ok);
    }
    let opened = knives::jj::Repo::open(&entry.path)?;
    let parents = opened.parent_commits(release_name)?;
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
    record_edit_event(
        repo,
        entry,
        &opened,
        &EditRecord {
            release: release_name,
            delta: &delta,
            created: &created,
            provenance: &provenance,
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
    let new_parents = reopened.parent_commits(rebased.name)?;
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
    // A rebase rewrites every member's commit id; without this the ledger's
    // last (branch, commit) pairing for the release would name commits no
    // longer among its parents, and the next edit could not tell a rebuilt
    // branch from a stranger.
    let delta = format!("rebased onto {}", rebased.reference);
    record_edit_event(
        repo,
        entry,
        &reopened,
        &EditRecord {
            release: rebased.name,
            delta: &delta,
            created: &described,
            provenance: &provenance,
        },
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
    match knives::jj::conflicted_files(&entry.path, described.as_str()) {
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
    let carried = carried_from_tips(&tips, entry.trunk(), scheme);
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
            commit.short().to_owned()
        };
        sources.push((source, commit.clone()));
    }
    Ok(sources)
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

/// The durable provenance required when a claim is released by force.
fn forced_release_event(
    claim: &knives::store::Claim,
    last_seen: knives::seen::LastSeen,
    why: &str,
) -> String {
    format!(
        "released {}'s claim by force ({}, claimed {}, last seen {}): {why}",
        claim.owner,
        knives::commands::claim::owner_kind_label(claim.kind),
        claim.started,
        last_seen_provenance(last_seen),
    )
}

struct FinishOptions<'a> {
    superseded_by: Option<&'a str>,
    cleanup: bool,
    force: bool,
    why: Option<&'a str>,
}

enum FinishClaimGate {
    Continue(Option<String>),
    Refuse,
}

/// Hand a branch back and remove its workspace. The inverse of `start`.
///
/// Removing the directory loses no work: jj snapshots a working copy into a commit, so
/// every change made there is already in the repository and reachable by change id. What
/// does not survive is anything jj never tracked, which is what `--no-cleanup` is for.
fn run_finish(target: &BranchTarget, options: &FinishOptions<'_>) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(&target.repo) else {
        eprintln!("unknown repo {}", target.repo);
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    let workspace = knives::commands::wip::workspace_for(target.branch.as_str());
    let directory = entry.path.parent().map(|parent| parent.join(&workspace));
    // The primary workspace is named "default" and the registered checkout is its
    // directory; `start` can never have created either for a branch, so a branch
    // whose flattened name lands on them is a collision, not a workspace to clean
    // up. Without this, `finish` would forget the primary workspace and delete
    // the checkout itself.
    if workspace == "default" || directory.as_deref() == Some(entry.path.as_path()) {
        eprintln!(
            "{}: branch {} maps to workspace {workspace}, which is the registered \
             checkout itself; refusing to touch {}",
            target.repo,
            target.branch,
            entry.path.display()
        );
        return Ok(Exit::Usage);
    }
    let forced_release = match finish_claim_gate(target, entry, &store, options)? {
        FinishClaimGate::Continue(event) => event,
        FinishClaimGate::Refuse => return Ok(Exit::Usage),
    };
    let had = store.release_claim(target);
    if let Some(new) = options.superseded_by {
        store.supersede(target, new);
    }
    let pr = store.tracked_pull(target);
    // Persist the immutable explanation before the mutable claim state. A failed
    // append then leaves the old claim in place instead of silently releasing or
    // seizing work without the provenance that explains why.
    let provenance = forced_release
        .map(|text| match options.superseded_by {
            Some(replacement) => format!("{text}; superseded by {replacement}"),
            None => text,
        })
        .or_else(|| release_event(had, options.superseded_by));
    if let Some(text) = provenance {
        scribe_for(&target.repo, entry)?.event(Some(target.branch.as_str()), text, pr)?;
    }
    store.save()?;

    let claim = if had { "released" } else { "was not held" };
    if let Err(error) = knives::jj::forget_workspace(&entry.path, &workspace) {
        println!("{target}: claim {claim}; no workspace forgotten ({error})");
        return Ok(Exit::Ok);
    }
    match (options.cleanup, directory) {
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

fn finish_claim_gate(
    target: &BranchTarget,
    entry: &knives::config::RepoEntry,
    store: &Store,
    options: &FinishOptions<'_>,
) -> anyhow::Result<FinishClaimGate> {
    let Some(claim) = store
        .claims(Some(&target.repo))
        .into_iter()
        .find(|claim| claim.branch == target.branch.as_str())
        .cloned()
    else {
        return Ok(FinishClaimGate::Continue(None));
    };
    let workspace = knives::commands::wip::workspace_for(target.branch.as_str());
    let directory = entry.path.parent().map(|parent| parent.join(&workspace));
    let cwd = std::env::current_dir()?;
    let identity = current_identity(&cwd)?;
    let decision = decide(&ClaimContext {
        held: Some(&claim),
        identity: &identity,
        in_claimed_workspace: directory.as_ref().is_some_and(|path| cwd.starts_with(path)),
    });
    match decision {
        ClaimDecision::Resume { .. } | ClaimDecision::Take => Ok(FinishClaimGate::Continue(None)),
        ClaimDecision::RefuseAnonymous | ClaimDecision::RefuseHeld => {
            let activity = Repo::open(&entry.path)?.workspace_activity(
                &std::collections::BTreeSet::from([knives::ids::WorkspaceName::new(workspace)]),
                knives::jj::MAX_ACTIVITY_OPS,
            )?;
            let last_seen = knives::seen::last_seen(&claim, &activity, &knives::seen::load());
            if !options.force {
                let anonymous_note = if decision == ClaimDecision::RefuseAnonymous {
                    "both sides are anonymous identities, so they can never match; "
                } else {
                    ""
                };
                eprintln!(
                    "{anonymous_note}{}\nuse `knives finish {} --force --why \"…\"` to release the claim",
                    render_claim_context(&claim, last_seen, jiff::Timestamp::now()),
                    target.branch,
                );
                return Ok(FinishClaimGate::Refuse);
            }
            let why = options
                .why
                .ok_or_else(|| anyhow::anyhow!("--force requires --why"))?;
            Ok(FinishClaimGate::Continue(Some(forced_release_event(
                &claim, last_seen, why,
            ))))
        }
    }
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

enum ReleaseInvocation {
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
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    for (repo, entry) in chosen {
        let opened = knives::jj::Repo::open(&entry.path)?;
        let scheme = entry.release_scheme();
        let (cut_name, allow_drop) = match requested_cut(invocation, &scheme) {
            Ok(request) => request,
            Err(exit) => return Ok(exit),
        };
        let plan_exit = release_plan_exit(
            &repo,
            &entry,
            &locals,
            &opened,
            &forge,
            cache_root.as_deref(),
            &heads,
        )?;
        worst = worst.worst(plan_exit);
        if plan_exit == Exit::Incomplete {
            continue;
        }

        if let Some(name) = cut_name {
            let trunk_name = entry.upstream_trunk();
            let trunk = opened.resolve_commit(&trunk_name)?;
            if let Some(orphaned) = check_orphan_commits_before_cut(&opened, &entry)?
                && let Some(exit) = report_orphaned_cut(&repo, &orphaned, allow_drop)
            {
                return Ok(exit);
            }
            let tips = opened.bookmark_tips()?;
            let previous = previous_release_for_cut(&entry, &tips);
            let previous_commit = previous.as_ref().map(|(_, commit)| commit.clone());
            // A cut is a new name for the composition in hand, never a recomputation:
            // with a previous release its parents are carried verbatim — nothing joins,
            // nothing advances, and a branch enters through `release include`. Only the
            // first cut has no composition to carry, so it starts from every branch: a
            // release is a flat merge of feature and fix branches, and the upstream
            // base is never a direct parent — it is reachable through every member.
            let (carried, members, audit_base) = if let Some((_, previous)) = &previous {
                let parents = opened.parent_commits(previous.as_str())?;
                let carried = parent_sources(&opened, &entry, &scheme, &parents)?;
                let members = carried.clone();
                let base = release::shared_base(&opened, previous, &trunk)?
                    .unwrap_or_else(|| trunk.clone());
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
                if refuse_stacked_first_cut(&repo, &opened, &entry, &carried)? {
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

#[allow(
    clippy::too_many_arguments,
    reason = "the release plan needs explicit repository state and each independently owned consumer-scan collaborator"
)]
fn release_plan_exit(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    locals: &[std::path::PathBuf],
    opened: &knives::jj::Repo,
    forge: &dyn knives::consumer_pins::ConsumerPinSource,
    cache_root: Option<&std::path::Path>,
    heads: &knives::consumer_pins::ConsumerHeadMemo,
) -> anyhow::Result<Exit> {
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals,
        forge,
        cache_root,
        heads,
    };
    let plan = release::plan(repo, entry, &consumers, &Ledger::for_repo(repo).entries()?)?;
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
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    cut: &CompletedCut<'_>,
) -> anyhow::Result<()> {
    let opened = knives::jj::Repo::open(&entry.path)?;
    let parents = opened.parent_commits(cut.created.as_str())?;
    let change = opened.change_id_of(cut.created.as_str())?;
    let members = parent_sources(&opened, entry, cut.scheme, &parents)?;
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
    scribe_for(repo, entry)?.record(&Draft {
        subject: Some(cut.name),
        kind: Kind::Event,
        disposition: None,
        text: format!(
            "cut {} as {} (change {}) with {} parent(s): {members_text}{delta}",
            cut.name,
            cut.created.short(),
            change.as_str().chars().take(12).collect::<String>(),
            members.len()
        ),
        evidence,
        pr: None,
        parents: recorded_parents(&opened, entry, &parents)?,
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
        "  cut {} as {} with {} parent(s), not pushed",
        cut.name,
        cut.created.short(),
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
        println!("    {}", commit.short());
    }
    println!("  re-run with --allow-drop to state this is intended");
    Some(Exit::Incomplete)
}

fn check_orphan_commits_before_cut(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
) -> anyhow::Result<Option<OrphanedLineage>> {
    let tips = opened.bookmark_tips()?;
    let Some(previous) = previous_release_for_cut(entry, &tips) else {
        return Ok(None);
    };
    let keep = release::cut_keepers(opened, entry, &tips, &previous.1)?;
    let orphans = release::orphaned_commits(release::OrphanedCommitInput {
        repo_path: &entry.path,
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

/// The cut for `carried`. A commit two bookmarks hold — a branch and an anchor
/// another agent left at its tip — is one parent, named twice in provenance.
fn cut_request(name: String, carried: &[(String, knives::ids::CommitId)]) -> release::Cut {
    let mut parents: Vec<knives::ids::CommitId> = Vec::with_capacity(carried.len());
    for (_, commit) in carried {
        if !parents.contains(commit) {
            parents.push(commit.clone());
        }
    }
    release::Cut {
        name,
        parents,
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
    let report = release::reap_superseded(&entry.path, &reopened, entry.publish_remote())?;
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
        let report = release::reap_superseded(&entry.path, &opened, entry.publish_remote())?;
        print_reap(&repo.to_string(), &report);
        worst = worst.worst(reap_exit(&report));
    }
    Ok(worst)
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
