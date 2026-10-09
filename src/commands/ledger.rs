//! `knives ledger sweep`: commit, pull and push this machine's ledger.
//!
//! Every command that appends an entry starts one detached as it exits
//! ([`crate::ledger_sweep::hand_off`]), so it is rarely typed; run by hand it
//! reports what it carried.

use crate::cli::{Exit, Output};
use crate::ledger_sweep::{self, Swept, Tally};

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
