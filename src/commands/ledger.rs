//! `knives ledger sweep`: commit, pull and push this machine's ledger.
//! `knives ledger migrate`: move what an older knives kept in the state file
//! onto the ledger.
//!
//! Every command that appends an entry starts one sweep detached as it exits
//! ([`crate::ledger_sweep::hand_off`]), so it is rarely typed; run by hand it
//! reports what it carried.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::cli::{Exit, Output};
use crate::ids::UpstreamName;
use crate::ledger::{Draft, Ledger, Scribe, inline_human_text};
use crate::ledger_sweep::{self, Swept, Tally};
use crate::statement::{Statement, StatementKind, Statements};
use crate::store::{LegacyStatement, Store};

/// What a sweep run by hand reports.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    /// `swept`, `busy` when another sweep holds the lock and carries this
    /// one's work, or `not-shared` when this machine's ledger has no
    /// repository to travel through.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<Tally>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Run one sweep of the ledger beside the registry and report it.
pub fn run_sweep(output: Output) -> anyhow::Result<Exit> {
    let root = crate::ledger::default_ledger_root();
    let report = match ledger_sweep::run(&root) {
        Ok(Swept::NotShared) => Report {
            outcome: "not-shared",
            destinations: Vec::new(),
            problems: Vec::new(),
        },
        Ok(Swept::Busy) => Report {
            outcome: "busy",
            destinations: Vec::new(),
            problems: Vec::new(),
        },
        Ok(Swept::Ran { tallies, problems }) => Report {
            outcome: "swept",
            destinations: tallies,
            problems,
        },
        Err(error) => Report {
            outcome: "swept",
            destinations: Vec::new(),
            problems: vec![error.to_string()],
        },
    };
    if let Some(payload) = crate::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", render(&report));
    }
    Ok(if report.problems.is_empty() {
        Exit::Ok
    } else {
        Exit::Incomplete
    })
}

pub fn render(report: &Report) -> String {
    let mut lines = match report.outcome {
        "not-shared" => vec![
            "ledger: not shared; this machine's ledger has no repository to travel through"
                .to_owned(),
        ],
        "busy" => {
            vec!["ledger: another sweep is running and carries this one's entries".to_owned()]
        }
        _ => report
            .destinations
            .iter()
            .map(|tally| {
                format!(
                    "ledger: {} ({}): {} commit(s), {} push(es), {} entr{} pulled; carries {}",
                    tally.remote,
                    tally.git_dir,
                    tally.commits,
                    tally.pushes,
                    tally.pulled,
                    if tally.pulled == 1 { "y" } else { "ies" },
                    if tally.forks.is_empty() {
                        "no fork".to_owned()
                    } else {
                        tally.forks.join(", ")
                    }
                )
            })
            .collect(),
    };
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("!! {problem}")),
    );
    lines.join("\n")
}

/// What a migration did.
#[derive(Debug, Default, serde::Serialize)]
pub struct Migrated {
    /// Statement entries this run appended to the ledger.
    pub wrote: usize,
    /// Statements the ledger already carried: an entry about that branch
    /// states that kind with that value.
    pub already: usize,
    /// What could not be migrated. The state file keeps every statement
    /// while there is one, so a run after the fix finishes the job.
    pub problems: Vec<String>,
}

/// Move every branch statement the state file at `state_path` holds onto the
/// ledger at `root`, one statement entry each, written as `owner`; then drop
/// the four maps that held them.
///
/// Nothing is parsed out of existing prose: an event reading `stated as #1234`
/// states nothing, and only an entry's `statement` field is a statement. A
/// statement some entry about that branch already makes, kind and value alike,
/// is skipped, so a second run writes nothing.
///
/// The state file is held for update throughout, so no other writer saves
/// over it between the read and the drop, and it is saved only when every
/// statement reached the ledger.
pub fn migrate(state_path: &Path, root: &Path, owner: &str) -> anyhow::Result<Migrated> {
    let mut store = Store::open_to_migrate(state_path.to_owned())?;
    let mut migrated = Migrated::default();
    let mut by_repo: BTreeMap<UpstreamName, Vec<(String, LegacyStatement)>> = BTreeMap::new();
    for legacy in store.legacy_statements()? {
        match legacy.key.split_once('/') {
            Some((repo, branch)) if !repo.is_empty() && !branch.is_empty() => by_repo
                .entry(UpstreamName::new(repo))
                .or_default()
                .push((branch.to_owned(), legacy)),
            _ => migrated.problems.push(format!(
                "{}: cannot read the key as <repo>/<branch>",
                legacy.key
            )),
        }
    }
    for (repo, statements) in by_repo {
        let ledger = Ledger::at(root.join(repo.as_str()));
        let scribe = Scribe::unanchored(ledger, repo, owner.to_owned());
        migrate_repo(&scribe, &statements, &mut migrated);
    }
    if migrated.problems.is_empty() && store.drop_legacy_statements() {
        store.save()?;
    }
    Ok(migrated)
}

/// One repository's statements onto its ledger, read once.
fn migrate_repo(
    scribe: &Scribe,
    statements: &[(String, LegacyStatement)],
    migrated: &mut Migrated,
) {
    let entries = match scribe.ledger().entries() {
        Ok(entries) => entries,
        Err(error) => {
            migrated.problems.push(format!(
                "{}: {} statement(s) not migrated: {error}",
                scribe.repo(),
                statements.len()
            ));
            return;
        }
    };
    let carried: BTreeSet<(&str, &Statement)> = entries
        .iter()
        .filter_map(|entry| Some((entry.subject.as_deref()?, entry.statement.as_ref()?)))
        .collect();
    let stated_pull: BTreeMap<&str, u64> = statements
        .iter()
        .filter(|(_, legacy)| legacy.statement.kind == StatementKind::Pull)
        .filter_map(|(branch, legacy)| {
            Some((
                branch.as_str(),
                legacy.statement.value.as_deref()?.parse().ok()?,
            ))
        })
        .collect();
    let live = Statements::from_entries(&entries);
    for (branch, legacy) in statements {
        if carried.contains(&(branch.as_str(), &legacy.statement)) {
            migrated.already += 1;
            continue;
        }
        let pr = stated_pull
            .get(branch.as_str())
            .copied()
            .or_else(|| live.pull(branch));
        let draft = Draft {
            statement: Some(legacy.statement.clone()),
            ..Draft::event(Some(branch), history(&legacy.statement), pr)
        };
        match scribe.record(&draft) {
            Ok(_) => migrated.wrote += 1,
            Err(error) => migrated
                .problems
                .push(format!("{}/{branch}: {error}", scribe.repo())),
        }
    }
}

/// The prose a migrated statement's entry carries: history for a reader,
/// never read back as the statement.
fn history(statement: &Statement) -> String {
    let value = statement.value.as_deref().unwrap_or_default();
    let said = match statement.kind {
        StatementKind::Pull => format!("stated as #{value}"),
        StatementKind::ForkOnly => {
            format!("stated as having no upstream pull request ({value})")
        }
        StatementKind::Superseded => format!("superseded by {value}"),
        StatementKind::Depends if value.is_empty() => "requires nothing".to_owned(),
        StatementKind::Depends => format!("requires {}", value.replace(',', ", ")),
    };
    format!("migrated from state.json: {said}")
}

/// Migrate the state file and ledger beside the registry, and report it.
pub fn run_migrate(output: Output) -> anyhow::Result<Exit> {
    let owner = crate::commands::claim::current_identity(None)?.owner;
    let migrated = migrate(
        &crate::store::default_state_path(),
        &crate::ledger::default_ledger_root(),
        &owner,
    )?;
    if let Some(payload) = crate::cli::machine_payload(output, &migrated)? {
        println!("{payload}");
    } else {
        println!("{}", render_migrated(&migrated));
    }
    Ok(if migrated.problems.is_empty() {
        Exit::Ok
    } else {
        Exit::Incomplete
    })
}

pub fn render_migrated(migrated: &Migrated) -> String {
    let mut lines = vec![format!(
        "ledger: wrote {} statement(s); {} already on the ledger",
        migrated.wrote, migrated.already
    )];
    if !migrated.problems.is_empty() {
        lines.push(
            "ledger: state.json keeps its statements until every one migrates; run again \
             after fixing"
                .to_owned(),
        );
    }
    lines.extend(
        migrated
            .problems
            .iter()
            .map(|problem| format!("!! {}", inline_human_text(problem))),
    );
    lines.join("\n")
}
