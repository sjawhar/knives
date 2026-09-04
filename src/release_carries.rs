//! `knives release members` and `knives carries`.
//!
//! Content questions about a release: which parents it has and who still
//! holds each, and whether a revision's net content is actually carried by a
//! release or the trunk — one revision, or a census over every maintained
//! branch. Each verb is a gather, an exit code and a renderer; the repository
//! is only read.

use knives::bind::Fork;
use knives::carriage::{
    self, CarriesReport, CensusOptions, CensusReport, CheckInput, Target, TargetCheck, TargetRole,
};
use knives::cli::{Exit, Output};
use knives::commands::release;
use knives::forge::Forge;
use knives::forge::github::CliForge;
use knives::jj::Repo;
use knives::ledger::Ledger;

use super::parallelism;

#[derive(Clone, Copy)]
pub(crate) struct CarriesInvocation<'a> {
    pub(crate) revision: Option<&'a str>,
    pub(crate) target: Option<&'a str>,
    pub(crate) all: bool,
    pub(crate) no_github: bool,
    pub(crate) output: Output,
}

#[derive(Clone, Copy)]
pub(crate) struct MembersInvocation<'a> {
    pub(crate) reference: Option<&'a str>,
    pub(crate) verify: bool,
    pub(crate) output: Output,
}

/// Inspect a release's direct parents and, on request, replay their content.
pub(crate) fn run_release_members(
    fork: &Fork<'_>,
    request: MembersInvocation<'_>,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let entry = fork.entry;
    let opened = Repo::open(&fork.checkout.path)?;
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
        let plan = release::plan(fork, &consumers, &Ledger::for_repo(repo).entries()?)?;
        let Some(reference) = plan.release else {
            println!("{repo}: no release to inspect; cut one first");
            return Ok(Exit::Incomplete);
        };
        std::borrow::Cow::Owned(reference)
    };
    let report = release::gather_members(&opened, fork, &reference, request.verify)?;
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
pub(crate) fn run_release_carries(
    fork: &Fork<'_>,
    request: CarriesInvocation<'_>,
) -> anyhow::Result<Exit> {
    if request.all {
        let forge = CliForge;
        let forge: Option<&dyn Forge> = (!request.no_github).then_some(&forge);
        let cache_root = knives::forge_cache::cache_root();
        let report = carriage::census(
            fork,
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
    run_revision_carries(fork, request, revision)
}

fn run_revision_carries(
    fork: &Fork<'_>,
    request: CarriesInvocation<'_>,
    revision: &str,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let entry = fork.entry;
    let opened = Repo::open(&fork.checkout.path)?;
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

fn carries_problem(
    repo: &knives::ids::RepoName,
    revision: String,
    problem: String,
) -> CarriesReport {
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
    format!("{revision} @ {}", tip.short())
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
