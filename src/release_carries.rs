//! `knives release members`.
//!
//! Content questions about a release, each a gather, an exit code and a
//! renderer over a read-only repository. Bare `members` asks which parents the
//! release has and who still holds each ([`run_release_members`]); `--carries
//! REV` asks the other direction, whether a revision's net content is actually
//! carried by a release or the trunk ([`run_revision_carries`]); `--census`
//! asks that of every maintained branch ([`run_release_census`]). The parser
//! keeps the three apart, so `main` calls each directly.

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

/// Inspect a release's direct parents and, on request, replay their content:
/// the release `reference` names, or the one in hand.
pub(crate) fn run_release_members(
    fork: &Fork<'_>,
    reference: Option<&str>,
    verify: bool,
    output: Output,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let entry = fork.entry;
    let opened = Repo::open(&fork.checkout.path)?;
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let reference = if let Some(reference) = reference {
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
    let report = release::gather_members(&opened, fork, &reference, verify)?;
    let exit = members_exit(&report, verify);
    print_members(&report, output)?;
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

/// Census every maintained branch against the live releases and upstream trunk.
pub(crate) fn run_release_census(
    fork: &Fork<'_>,
    no_github: bool,
    output: Output,
) -> anyhow::Result<Exit> {
    let forge = CliForge;
    let forge: Option<&dyn Forge> = (!no_github).then_some(&forge);
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
    print_census(&report, output)?;
    Ok(exit)
}

/// Answer one revision's carriage: against `target` alone when given,
/// otherwise against every live release and the upstream trunk.
pub(crate) fn run_revision_carries(
    fork: &Fork<'_>,
    revision: &str,
    target: Option<&str>,
    output: Output,
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
            print_carries(&report, output)?;
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
            print_carries(&report, output)?;
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
    let (mut selected, superseded) = match target {
        Some(target) => match selected_carries_targets(&opened, &all_targets, target) {
            Ok(targets) => (targets, Vec::new()),
            Err(error) => {
                let report = carries_problem(
                    repo,
                    carries_revision(revision, &tip),
                    format!("cannot resolve target {target}: {error}"),
                );
                print_carries(&report, output)?;
                return Ok(Exit::Incomplete);
            }
        },
        None => all_targets
            .into_iter()
            .partition(|target| target.role != TargetRole::SupersededRelease),
    };
    if let (Some(requested), Some(selected_target)) = (target, selected.first_mut())
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
        fallback: target.unwrap_or(trunk_name.as_str()),
    };
    let mut report = CarriesReport {
        repo: repo.to_string(),
        revision: carries_revision(revision, &tip),
        checks: Vec::with_capacity(selected.len() + superseded.len()),
        notes: Vec::new(),
        problems: Vec::new(),
    };
    checks.append(&mut report, selected);
    let exit = carries_exit(&mut report, &checks, superseded, target);
    print_carries(&report, output)?;
    Ok(exit)
}

/// The outcome: unanswered checks outrank everything; against an explicit
/// `target` every check must carry; bare, a live release or the trunk must,
/// with the superseded releases consulted only after those miss.
fn carries_exit(
    report: &mut CarriesReport,
    checks: &CarriesChecks<'_>,
    superseded: Vec<Target>,
    target: Option<&str>,
) -> Exit {
    if !report.problems.is_empty() {
        return Exit::Incomplete;
    }
    let carried = if target.is_some() {
        report.checks.iter().all(|check| check.verdict.carried())
    } else {
        if !carries_safe(report) {
            checks.append(report, superseded);
        }
        carries_safe(report)
    };
    if carried { Exit::Ok } else { Exit::Findings }
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
