//! `knives ledger sweep` and `knives ledger migrate`.
//!
//! `sweep` commits, pulls and pushes this machine's ledger. `migrate` moves
//! what an older knives kept in the state file onto the ledger, and every
//! fork from the registry key an older knives named it by to its upstream
//! repository's name.
//!
//! Every command that appends an entry starts one sweep detached as it exits
//! ([`crate::ledger_sweep::hand_off`]), so it is rarely typed; run by hand it
//! reports what it carried.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::cli::{Exit, Output};
use crate::ids::{Requirement, UpstreamName};
use crate::ledger::{Draft, Ledger, Scribe, holds_entries, inline_human_text};
use crate::ledger_sweep::{self, Swept, Tally};
use crate::lock::FileLock;
use crate::statement::{Statement, StatementKind, Statements};
use crate::store::{FormerNames, LegacyStatement, Renamed, Store, renamed};

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
                let discarded = match tally.discarded {
                    0 => String::new(),
                    1 => ", 1 repeated transition discarded".to_owned(),
                    count => format!(", {count} repeated transitions discarded"),
                };
                format!(
                    "ledger: {} ({}): {} commit(s), {} push(es), {} entr{} pulled{discarded}; \
                     carries {}",
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
    /// Each fork's ledger directory moved from its registry key to its
    /// upstream name, by path.
    pub moved: Vec<Moved>,
    /// Each state-file key moved from a fork's registry key to its upstream
    /// name.
    pub renamed: Vec<Renamed>,
    /// Each `seen.json` workspace sighting moved the same way.
    pub sightings: Vec<crate::seen::RenamedSighting>,
    /// Statement entries this run appended to the ledger.
    pub wrote: usize,
    /// Statements the ledger already carried: an entry about that branch
    /// states that kind with that value.
    pub already: usize,
    /// What could not be migrated. The state file keeps every statement
    /// while a statement could not reach the ledger, so a run after the fix
    /// finishes the job.
    pub problems: Vec<String>,
    /// Whether the state file kept its statements because one of them could
    /// not reach the ledger.
    #[serde(skip)]
    pub statements_held: bool,
}

/// One ledger directory a migration moved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Moved {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Move what an older knives left at `state_path` and `root` onto what this
/// one reads.
///
/// Every fork the state file and the ledger keep under a registry key an
/// older knives named it by moves to its upstream name, as the registry
/// beside the state file says; then every branch statement the state file
/// holds moves onto the ledger, one statement entry each, written as
/// `owner`, and the four maps that held them are dropped.
///
/// A fork's ledger directory is moved file by file, contents untouched. An
/// entry already at its new name with the same bytes (pulled from a machine
/// that migrated first) is the same entry, and the old copy goes; one with
/// other bytes is a problem, and both stay for a person to look at.
///
/// Nothing is parsed out of existing prose: an event reading `stated as #1234`
/// states nothing, and only an entry's `statement` field is a statement. A
/// statement some entry about that branch already makes, kind and value alike,
/// is skipped, so a second run writes nothing. A dependency an older knives
/// recorded by a registry key is written under that fork's upstream name.
///
/// The state file is held for update throughout, so no other writer saves
/// over it between the read and the drop, and it is saved only when every
/// statement reached the ledger and is on disk: each written entry, and each
/// directory up to the config home, is flushed first, so a crash between the
/// save and the kernel's own writeback cannot lose a statement from both
/// places. The ledger's sweep lock is held while its
/// directories move, so no sweep commits a half-moved fork. The workspace
/// sightings in `seen.json` beside the state file move last, under its own
/// lock: a sidecar that cannot be read is a problem, but one that never
/// holds the statements back.
pub fn migrate(state_path: &Path, root: &Path, owner: &str) -> anyhow::Result<Migrated> {
    let mut store = Store::open_to_migrate(state_path.to_owned())?;
    let former = crate::config::load(&state_path.with_file_name("repos.toml"))?.former_names();
    let mut migrated = Migrated::default();
    if former.keys().any(|name| holds_entries(&root.join(name))) {
        let _sweep = FileLock::acquire(root, crate::lock::LockWait::TRANSPORT)?;
        move_ledgers(root, &former, &mut migrated);
    }
    let (renamed, refused) = store.rename_forks(&former);
    migrated.renamed = renamed;
    migrated.problems.extend(refused.iter().map(|refused| {
        format!(
            "{} {}: not moved to {}, which is already there",
            refused.map, refused.from, refused.to
        )
    }));
    let mut by_repo: BTreeMap<UpstreamName, Vec<(String, LegacyStatement)>> = BTreeMap::new();
    for legacy in store.legacy_statements()? {
        match legacy.key.split_once('/') {
            Some((repo, branch)) if !repo.is_empty() && !branch.is_empty() => by_repo
                .entry(
                    former
                        .get(repo)
                        .cloned()
                        .unwrap_or_else(|| UpstreamName::new(repo)),
                )
                .or_default()
                .push((branch.to_owned(), with_current_names(legacy, &former))),
            _ => migrated.problems.push(format!(
                "{}: cannot read the key as <repo>/<branch>",
                legacy.key
            )),
        }
    }
    let mut written = Vec::new();
    for (repo, statements) in by_repo {
        let ledger = Ledger::at(root.join(repo.as_str()));
        let scribe = Scribe::unanchored(ledger, repo, owner.to_owned());
        migrate_repo(&scribe, &statements, &mut migrated, &mut written);
    }
    migrated.statements_held = !migrated.problems.is_empty();
    if !migrated.statements_held
        && let Err(error) = flush(root, &written, &migrated.moved)
    {
        migrated.problems.push(format!(
            "flushing the migrated entries to disk: {error}; state.json keeps every statement"
        ));
        migrated.statements_held = true;
    }
    let dropped = !migrated.statements_held && store.drop_legacy_statements();
    if dropped || !migrated.renamed.is_empty() {
        store.save()?;
    }
    drop(store);
    match crate::seen::rename_workspaces(&state_path.with_file_name("seen.json"), &former) {
        Ok(sightings) => migrated.sightings = sightings,
        Err(error) => migrated
            .problems
            .push(format!("workspace sightings not renamed: {error}")),
    }
    Ok(migrated)
}

/// `legacy` with the repository of each requirement it states renamed from
/// a former name to the one its fork is kept under now.
fn with_current_names(legacy: LegacyStatement, former: &FormerNames) -> LegacyStatement {
    if legacy.statement.kind != StatementKind::Depends {
        return legacy;
    }
    let value = legacy.statement.value.as_deref().map(|value| {
        value
            .split(',')
            .map(|written| {
                Requirement::parse(written)
                    .and_then(|requirement| former.get(requirement.repo.as_str()).map(|_| written))
                    .and_then(|written| renamed(former, written, '#'))
                    .unwrap_or_else(|| written.to_owned())
            })
            .collect::<Vec<_>>()
            .join(",")
    });
    LegacyStatement {
        statement: Statement {
            kind: legacy.statement.kind,
            value,
        },
        ..legacy
    }
}

/// Move each `<root>/<former name>/` entry file into `<root>/<upstream name>/`,
/// recording each directory that moved and each file that could not.
///
/// Forks are moved in name order. A former name can be the owner directory
/// another fork's upstream name now lives in (`acme` the fork beside
/// `acme/demo`): its entry files are the regular files directly inside it,
/// and the directories there are other forks', left alone.
fn move_ledgers(root: &Path, former: &FormerNames, migrated: &mut Migrated) {
    for (name, now) in former {
        let from = root.join(name);
        if !holds_entries(&from) {
            continue;
        }
        let to = root.join(now.as_str());
        match move_entries(&from, &to) {
            Ok(clashes) if clashes.is_empty() => {
                // Empty now unless it holds another fork's directory.
                let _ = std::fs::remove_dir(&from);
                migrated.moved.push(Moved { from, to });
            }
            Ok(clashes) => migrated.problems.extend(clashes.into_iter().map(|file| {
                format!(
                    "{}: not moved, because {} holds a different entry under that name",
                    from.join(&file).display(),
                    to.join(&file).display()
                )
            })),
            Err(error) => migrated.problems.push(format!(
                "moving {} to {}: {error}",
                from.display(),
                to.display()
            )),
        }
    }
}

/// The regular files directly in `from` moved into `to`, created if absent;
/// the names left behind because `to` holds a different file under them.
fn move_entries(from: &Path, to: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    std::fs::create_dir_all(to)?;
    let mut clashes = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(from)?
        .map(|dirent| dirent.map(|dirent| (dirent.file_name(), dirent.file_type())))
        .collect::<Result<_, _>>()?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    for (file, kind) in files {
        if !kind?.is_file() {
            continue;
        }
        let (source, destination) = (from.join(&file), to.join(&file));
        if destination.exists() {
            if std::fs::read(&source)? == std::fs::read(&destination)? {
                std::fs::remove_file(&source)?;
            } else {
                clashes.push(file);
            }
        } else {
            std::fs::rename(&source, &destination)?;
        }
    }
    Ok(clashes)
}

/// Flush to disk each entry in `written`, then every directory holding one or
/// named in `moved`, and each directory above them up to the config home
/// holding `root`: a file's data and a directory's new names reach the disk
/// only on the kernel's own schedule otherwise, and the caller is about to
/// drop the only other copy of what they hold.
fn flush(root: &Path, written: &[PathBuf], moved: &[Moved]) -> std::io::Result<()> {
    let home = root.parent().unwrap_or(root);
    let mut directories = BTreeSet::new();
    for path in written {
        std::fs::File::open(path)?.sync_all()?;
    }
    let holding = written.iter().filter_map(|path| path.parent());
    let named = moved
        .iter()
        .flat_map(|moved| [moved.from.as_path(), moved.to.as_path()]);
    for directory in holding.chain(named) {
        for directory in directory.ancestors() {
            directories.insert(directory.to_owned());
            if directory == home {
                break;
            }
        }
    }
    for directory in directories {
        match std::fs::File::open(&directory) {
            Ok(handle) => handle.sync_all()?,
            // A former name's directory is removed once emptied.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// One repository's statements onto its ledger, read once; each entry file
/// written is added to `written`.
fn migrate_repo(
    scribe: &Scribe,
    statements: &[(String, LegacyStatement)],
    migrated: &mut Migrated,
    written: &mut Vec<PathBuf>,
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
        match scribe.record_to(&draft) {
            Ok((_, path)) => {
                migrated.wrote += 1;
                written.push(path);
            }
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
        // state.json held only the four kinds above; this arm is never taken.
        StatementKind::Unknown => "a statement of a kind this knives does not know".to_owned(),
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
    let mut lines: Vec<String> =
        migrated
            .moved
            .iter()
            .map(|moved| {
                format!(
                    "ledger: moved {} to {}",
                    moved.from.display(),
                    moved.to.display()
                )
            })
            .chain(migrated.renamed.iter().map(|renamed| {
                format!(
                    "state: renamed {} {} to {}",
                    renamed.map, renamed.from, renamed.to
                )
            }))
            .chain(migrated.sightings.iter().map(|renamed| {
                format!("seen: renamed workspace {} to {}", renamed.from, renamed.to)
            }))
            .collect();
    lines.push(format!(
        "ledger: wrote {} statement(s); {} already on the ledger",
        migrated.wrote, migrated.already
    ));
    if migrated.statements_held {
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
